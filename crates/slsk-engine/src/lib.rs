//! A Soulseek client engine on one tokio runtime.
//!
//! Nothing here holds a thread per peer or per transfer: each connection is a
//! pair of tasks, shared state lives in sharded maps, and every queue between
//! tasks is bounded, so memory follows active work rather than history. The
//! paths that run for every search on the network — the share index and the
//! forward to distributed children — do not allocate per child and do not
//! take a global lock.

mod distributed;
mod downloads;
mod limit;
mod metrics;
mod net;
mod peers;
mod server;
pub mod shares;
mod uploads;

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::RwLock;
use slsk_proto::RawStr;
use slsk_proto::peer::{Directory, PeerMessage, SearchResponse, UserInfo};
use slsk_proto::server::{FromServer, ToServer};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc, oneshot, watch};
use zeroize::Zeroizing;

pub use downloads::DownloadState;
pub use metrics::Metrics;
pub use slsk_proto;
pub use uploads::UploadState;

#[derive(Clone)]
pub struct EngineConfig {
    pub username: String,
    pub password: String,
    pub server: String,
    /// 0 binds whatever port the system assigns; the bound port is what is
    /// announced to the server.
    pub listen_port: u16,
    pub share_dirs: Vec<PathBuf>,
    /// Where the audio-probe cache lives between scans.
    pub state_dir: PathBuf,
    pub upload_slots: usize,
    /// Bytes per second, 0 for unlimited.
    pub upload_limit: u64,
    pub download_limit: u64,
    pub accept_children: bool,
    pub max_children: usize,
    pub max_search_results: usize,
    /// Concurrent outgoing search responses. Beyond this, requests are
    /// dropped rather than queued: a search answered a minute late is noise.
    pub max_search_responders: usize,
    pub max_queued_files_per_user: usize,
    pub max_queued_bytes_per_user: u64,
    pub description: String,
}

impl EngineConfig {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            server: "server.slsknet.org:2242".into(),
            listen_port: 2234,
            share_dirs: Vec::new(),
            state_dir: std::env::temp_dir(),
            upload_slots: 5,
            upload_limit: 0,
            download_limit: 0,
            accept_children: true,
            max_children: 10,
            max_search_results: 300,
            max_search_responders: 64,
            max_queued_files_per_user: 2000,
            max_queued_bytes_per_user: 50 << 30,
            description: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    LoggedIn {
        own_ip: Ipv4Addr,
    },
    /// The server refused the login. Not retried until credentials change.
    Rejected {
        reason: String,
    },
    /// Another client logged in with this account. Not retried until asked,
    /// or the two would take turns kicking each other off.
    Displaced,
    Disconnected {
        error: String,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Every server message, after the engine has acted on the ones it owns
    /// (addresses, indirect connections, distributed search). The app layer
    /// takes rooms, private messages, user status and the rest from here.
    Server(Arc<FromServer>),
    Status(Status),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

#[derive(Debug, Clone)]
pub struct TransferView {
    pub id: u64,
    pub direction: Direction,
    pub username: String,
    pub filename: RawStr,
    pub size: u64,
    pub bytes: u64,
    /// Bytes per second over the last second or so.
    pub speed: u64,
    pub state: &'static str,
    pub place: Option<u32>,
    pub error: Option<String>,
    /// Where a download is written.
    pub path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not logged in")]
    NotConnected,
    #[error("{0} is offline")]
    Offline(String),
    #[error("could not reach {0}")]
    Unreachable(String),
    #[error("timed out")]
    TimedOut,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

type Key = (String, Bytes);

pub(crate) struct Inner {
    cfg: EngineConfig,
    credentials: RwLock<(String, Zeroizing<String>)>,
    server_tx: RwLock<Option<mpsc::Sender<Bytes>>>,
    status: watch::Sender<Status>,
    events: broadcast::Sender<Event>,
    reconnect: Notify,
    shares: shares::Shares,
    excluded: RwLock<Vec<String>>,
    banned: RwLock<HashSet<String>>,
    privileged: RwLock<HashSet<String>>,
    token: AtomicU32,
    peers: peers::Peers,
    searches: DashMap<u32, mpsc::Sender<SearchResponse>>,
    browse_waiters: DashMap<String, Vec<oneshot::Sender<Vec<Directory>>>>,
    folder_waiters: DashMap<u32, oneshot::Sender<Vec<Directory>>>,
    info_waiters: DashMap<String, Vec<oneshot::Sender<UserInfo>>>,
    downloads: downloads::Downloads,
    uploads: uploads::Uploads,
    dist: distributed::Dist,
    responders: Arc<Semaphore>,
    pub(crate) metrics: Arc<Metrics>,
    up_limit: limit::Bucket,
    down_limit: limit::Bucket,
}

impl Inner {
    fn username(&self) -> String {
        self.credentials.read().0.clone()
    }

    fn next_token(&self) -> u32 {
        self.token.fetch_add(1, Ordering::Relaxed)
    }

    fn send_server(&self, msg: ToServer) -> Result<()> {
        let tx = self.server_tx.read().clone().ok_or(Error::NotConnected)?;
        tx.try_send(msg.encode()).map_err(|_| Error::NotConnected)
    }

    fn logged_in(&self) -> bool {
        matches!(*self.status.borrow(), Status::LoggedIn { .. })
    }

    fn is_banned(&self, username: &str) -> bool {
        self.banned.read().contains(&username.to_lowercase())
    }

    fn set_status(&self, status: Status) {
        self.metrics.logged_in.store(
            u64::from(matches!(status, Status::LoggedIn { .. })),
            Ordering::Relaxed,
        );
        self.status.send_replace(status.clone());
        let _ = self.events.send(Event::Status(status));
    }
}

/// A handle to a running engine. Cheap to clone.
#[derive(Clone)]
pub struct Engine(Arc<Inner>);

impl Engine {
    /// Bind the listener, start scanning shares, and start logging in. Returns
    /// once the listener is bound; login progress is on [`Engine::status`].
    pub async fn start(cfg: EngineConfig) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.listen_port)).await?;
        let mut cfg = cfg;
        cfg.listen_port = listener.local_addr()?.port();
        let (status, _) = watch::channel(Status::Connecting);
        let (events, _) = broadcast::channel(4096);
        let metrics = Arc::new(Metrics::default());
        let inner = Arc::new(Inner {
            credentials: RwLock::new((cfg.username.clone(), Zeroizing::new(cfg.password.clone()))),
            server_tx: RwLock::new(None),
            status,
            events,
            reconnect: Notify::new(),
            shares: Arc::new(arc_swap::ArcSwap::from_pointee(shares::ShareIndex::empty())),
            excluded: RwLock::new(Vec::new()),
            banned: RwLock::new(HashSet::new()),
            privileged: RwLock::new(HashSet::new()),
            token: AtomicU32::new(rand::random::<u32>() >> 1),
            peers: peers::Peers::default(),
            searches: DashMap::new(),
            browse_waiters: DashMap::new(),
            folder_waiters: DashMap::new(),
            info_waiters: DashMap::new(),
            downloads: downloads::Downloads::default(),
            uploads: uploads::Uploads::new(cfg.upload_slots),
            dist: distributed::Dist::new(cfg.max_children),
            responders: Arc::new(Semaphore::new(cfg.max_search_responders)),
            up_limit: limit::Bucket::new(cfg.upload_limit),
            down_limit: limit::Bucket::new(cfg.download_limit),
            metrics,
            cfg,
        });
        tokio::spawn(peers::listen(inner.clone(), listener));
        tokio::spawn(server::run(inner.clone()));
        tokio::spawn(uploads::schedule(inner.clone()));
        let engine = Self(inner);
        let scanner = engine.clone();
        tokio::spawn(async move { scanner.rescan().await });
        Ok(engine)
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.0.status.subscribe()
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.0.metrics.clone()
    }

    pub fn username(&self) -> String {
        self.0.username()
    }

    /// The port peers connect to.
    pub fn listen_port(&self) -> u16 {
        self.0.cfg.listen_port
    }

    /// Log in as someone else, or with a new password. Also the way out of
    /// [`Status::Rejected`] and [`Status::Displaced`].
    pub fn set_credentials(&self, username: &str, password: &str) {
        *self.0.credentials.write() = (username.to_string(), Zeroizing::new(password.to_string()));
        self.reconnect();
    }

    pub fn password_matches(&self, password: &str) -> bool {
        self.0.credentials.read().1.as_str() == password
    }

    /// Drop the server connection and log in again.
    pub fn reconnect(&self) {
        self.0.server_tx.write().take();
        self.0.reconnect.notify_one();
    }

    /// Send any server message. Rooms, private messages, interests and the
    /// rest of the social surface go through here; their replies arrive as
    /// [`Event::Server`].
    pub fn send(&self, msg: ToServer) -> Result<()> {
        self.0.send_server(msg)
    }

    pub fn set_banned(&self, users: impl IntoIterator<Item = String>) {
        let set: HashSet<String> = users.into_iter().map(|u| u.to_lowercase()).collect();
        self.0.uploads.drop_users(&set);
        *self.0.banned.write() = set;
    }

    pub fn set_upload_slots(&self, slots: usize) {
        self.0.uploads.set_slots(slots);
    }

    pub fn set_limits(&self, upload: u64, download: u64) {
        self.0.up_limit.set_rate(upload);
        self.0.down_limit.set_rate(download);
    }

    /// Rescan shared directories and announce the new counts. Only changed
    /// files are probed, so this is cheap to call after every import.
    pub async fn rescan(&self) {
        // Twice when the cache is cold: once from what is already known, so
        // everything is shared within seconds, then again once every new
        // file's audio headers have been read.
        self.scan_and_publish(false).await;
        self.scan_and_publish(true).await;
    }

    async fn scan_and_publish(&self, probe: bool) {
        let inner = self.0.clone();
        let dirs = inner.cfg.share_dirs.clone();
        let cache_path = inner.cfg.state_dir.join("probe-cache.bin");
        let started = std::time::Instant::now();
        let index = tokio::task::spawn_blocking(move || {
            let mut cache = shares::ProbeCache::load(&cache_path);
            let save = |c: &shares::ProbeCache| {
                if let Err(e) = c.save(&cache_path) {
                    tracing::warn!(error = %e, "could not save the probe cache");
                }
            };
            let index = shares::scan(&dirs, &mut cache, probe, save);
            save(&cache);
            index
        })
        .await;
        let Ok(index) = index else {
            tracing::error!("share scan panicked; keeping the previous index");
            return;
        };
        tracing::info!(files = index.file_count(), dirs = index.dir_count(), probed = probe, took = ?started.elapsed(), "shares scanned");
        let m = &self.0.metrics;
        m.shared_files
            .store(index.file_count() as u64, Ordering::Relaxed);
        m.shared_folders
            .store(index.dir_count() as u64, Ordering::Relaxed);
        m.shared_bytes.store(index.total_bytes(), Ordering::Relaxed);
        let (dirs, files) = (index.dir_count() as u32, index.file_count() as u32);
        self.0.shares.store(Arc::new(index));
        let _ = self
            .0
            .send_server(ToServer::SharedFoldersFiles { dirs, files });
    }

    pub fn share_counts(&self) -> (usize, usize) {
        let index = self.0.shares.load();
        (index.dir_count(), index.file_count())
    }

    /// Search the network. Responses arrive on the receiver until it is
    /// dropped; the network gives no end-of-results signal, so the caller
    /// decides how long to listen.
    pub fn search(&self, query: &str) -> Result<mpsc::Receiver<SearchResponse>> {
        self.search_with(|token| ToServer::FileSearch {
            token,
            query: query.to_string(),
        })
    }

    pub fn search_user(
        &self,
        username: &str,
        query: &str,
    ) -> Result<mpsc::Receiver<SearchResponse>> {
        self.search_with(|token| ToServer::UserSearch {
            username: username.to_string(),
            token,
            query: query.to_string(),
        })
    }

    pub fn search_room(&self, room: &str, query: &str) -> Result<mpsc::Receiver<SearchResponse>> {
        self.search_with(|token| ToServer::RoomSearch {
            room: room.to_string(),
            token,
            query: query.to_string(),
        })
    }

    pub fn wishlist_search(&self, query: &str) -> Result<mpsc::Receiver<SearchResponse>> {
        self.search_with(|token| ToServer::WishlistSearch {
            token,
            query: query.to_string(),
        })
    }

    fn search_with(
        &self,
        msg: impl FnOnce(u32) -> ToServer,
    ) -> Result<mpsc::Receiver<SearchResponse>> {
        let token = self.0.next_token();
        let (tx, rx) = mpsc::channel(1024);
        self.0.searches.insert(token, tx);
        if let Err(e) = self.0.send_server(msg(token)) {
            self.0.searches.remove(&token);
            return Err(e);
        }
        self.0.metrics.searches_sent.fetch_add(1, Ordering::Relaxed);
        Ok(rx)
    }

    pub async fn browse(&self, username: &str) -> Result<Vec<Directory>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .browse_waiters
            .entry(username.to_string())
            .or_default()
            .push(tx);
        peers::send(&self.0, username, PeerMessage::GetSharedFileList.encode()).await?;
        tokio::time::timeout(Duration::from_secs(180), rx)
            .await
            .map_err(|_| Error::TimedOut)?
            .map_err(|_| Error::TimedOut)
    }

    pub async fn folder_contents(&self, username: &str, folder: &RawStr) -> Result<Vec<Directory>> {
        let token = self.0.next_token();
        let (tx, rx) = oneshot::channel();
        self.0.folder_waiters.insert(token, tx);
        let msg = PeerMessage::FolderContentsRequest {
            token,
            folder: folder.clone(),
        };
        let result = async {
            peers::send(&self.0, username, msg.encode()).await?;
            tokio::time::timeout(Duration::from_secs(60), rx)
                .await
                .map_err(|_| Error::TimedOut)?
                .map_err(|_| Error::TimedOut)
        }
        .await;
        self.0.folder_waiters.remove(&token);
        result
    }

    pub async fn user_info(&self, username: &str) -> Result<UserInfo> {
        let (tx, rx) = oneshot::channel();
        self.0
            .info_waiters
            .entry(username.to_string())
            .or_default()
            .push(tx);
        peers::send(&self.0, username, PeerMessage::UserInfoRequest.encode()).await?;
        tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .map_err(|_| Error::TimedOut)?
            .map_err(|_| Error::TimedOut)
    }

    /// Queue a download. `dest` is the final path; data lands beside it as
    /// `.part` until complete, and an existing `.part` is resumed.
    pub fn download(&self, username: &str, filename: RawStr, size: u64, dest: PathBuf) -> u64 {
        downloads::add(&self.0, username, filename, size, dest)
    }

    pub fn cancel_download(&self, username: &str, filename: &RawStr) -> bool {
        self.0.downloads.cancel(username, filename)
    }

    pub fn retry_download(&self, username: &str, filename: &RawStr) -> bool {
        downloads::retry(&self.0, username, filename)
    }

    pub fn remove_download(&self, username: &str, filename: &RawStr) -> bool {
        self.0.downloads.remove(username, filename)
    }

    pub fn cancel_upload(&self, username: &str, filename: &RawStr) -> bool {
        self.0.uploads.cancel(username, filename)
    }

    pub fn downloads(&self) -> Vec<TransferView> {
        self.0.downloads.views()
    }

    pub fn uploads(&self) -> Vec<TransferView> {
        self.0.uploads.views()
    }

    pub fn download_view(&self, username: &str, filename: &RawStr) -> Option<TransferView> {
        self.0.downloads.view(username, filename)
    }

    pub fn distributed(&self) -> (Option<String>, i32, String, usize) {
        self.0.dist.summary()
    }
}
