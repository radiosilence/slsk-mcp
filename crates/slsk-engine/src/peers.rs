//! Peer connections: making them, accepting them, and what arrives on them.
//!
//! An outgoing connection races both routes the modern protocol allows at
//! once — a direct connect to the address the server gives, and an indirect
//! request asking the peer to connect back and pierce our firewall — and
//! keeps whichever lands first. Doing them in sequence, as older clients do,
//! costs a full timeout for every firewalled peer.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use slsk_proto::peer::{PeerInit, PeerMessage, SearchResponse};
use slsk_proto::server::ToServer;
use slsk_proto::{CodeWidth, ConnKind, reason};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::net::{self, Conn};
use crate::{Error, Inner, Result, distributed, downloads, uploads};

/// A peer connection that carries nothing for this long is closed. Peers
/// reconnect on demand, and an idle socket is a file descriptor and two
/// tasks for nothing.
const IDLE: Duration = Duration::from_secs(600);
/// Share lists of very large collections are tens of megabytes compressed.
const MAX_PEER_FRAME: usize = 128 << 20;
const DIRECT_TIMEOUT: Duration = Duration::from_secs(8);
const INDIRECT_TIMEOUT: Duration = Duration::from_secs(30);
const ADDRESS_TTL: Duration = Duration::from_secs(600);

pub(crate) struct Link {
    id: u64,
    pub tx: mpsc::Sender<Bytes>,
}

#[derive(Default)]
pub(crate) struct Peers {
    pub links: DashMap<String, Link>,
    connecting: DashMap<String, Arc<Mutex<()>>>,
    addresses: DashMap<String, (SocketAddrV4, Instant)>,
    address_waiters: DashMap<String, Vec<oneshot::Sender<SocketAddrV4>>>,
    pierce: DashMap<u32, oneshot::Sender<Option<Conn>>>,
    next_id: AtomicU64,
}

pub(crate) fn on_address(inner: &Inner, username: &str, ip: Ipv4Addr, port: u32) {
    let addr = SocketAddrV4::new(ip, port as u16);
    if port != 0 && !ip.is_unspecified() {
        inner
            .peers
            .addresses
            .insert(username.to_string(), (addr, Instant::now()));
    }
    if let Some((_, waiters)) = inner.peers.address_waiters.remove(username) {
        for w in waiters {
            let _ = w.send(addr);
        }
    }
}

pub(crate) fn on_cant_connect(inner: &Inner, token: u32) {
    if let Some((_, tx)) = inner.peers.pierce.remove(&token) {
        let _ = tx.send(None);
    }
}

async fn address(inner: &Inner, username: &str) -> Result<SocketAddrV4> {
    if let Some(entry) = inner.peers.addresses.get(username)
        && entry.1.elapsed() < ADDRESS_TTL
    {
        return Ok(entry.0);
    }
    let (tx, rx) = oneshot::channel();
    inner
        .peers
        .address_waiters
        .entry(username.to_string())
        .or_default()
        .push(tx);
    inner.send_server(ToServer::GetPeerAddress {
        username: username.to_string(),
    })?;
    let addr = tokio::time::timeout(Duration::from_secs(15), rx)
        .await
        .map_err(|_| Error::TimedOut)?
        .map_err(|_| Error::TimedOut)?;
    if addr.port() == 0 || addr.ip().is_unspecified() {
        return Err(Error::Offline(username.to_string()));
    }
    Ok(addr)
}

/// Open a connection of `kind` to `username`, direct or pierced, whichever
/// succeeds first. The peer init has been exchanged when this returns.
pub(crate) async fn connect(inner: &Arc<Inner>, username: &str, kind: ConnKind) -> Result<Conn> {
    let token = inner.next_token();
    let (tx, rx) = oneshot::channel();
    inner.peers.pierce.insert(token, tx);
    let own = inner.username();
    let indirect_requested = inner
        .send_server(ToServer::ConnectToPeer {
            token,
            username: username.to_string(),
            kind,
        })
        .is_ok();

    let direct = async {
        let addr = address(inner, username).await?;
        let stream = net::connect(SocketAddr::V4(addr), DIRECT_TIMEOUT).await?;
        let mut conn = Conn::new(stream);
        conn.stream
            .write_all(
                &PeerInit::PeerInit {
                    username: own.clone(),
                    kind,
                    token: 0,
                }
                .encode(),
            )
            .await?;
        Ok::<_, Error>(conn)
    };
    let indirect = async {
        if !indirect_requested {
            return Err(Error::NotConnected);
        }
        match tokio::time::timeout(INDIRECT_TIMEOUT, rx).await {
            Ok(Ok(Some(conn))) => Ok(conn),
            Ok(_) => Err(Error::Unreachable(username.to_string())),
            Err(_) => Err(Error::TimedOut),
        }
    };
    tokio::pin!(direct, indirect);
    let result = tokio::select! {
        r = &mut direct => match r {
            Ok(c) => Ok(c),
            Err(Error::Offline(u)) => Err(Error::Offline(u)),
            Err(_) => indirect.await,
        },
        r = &mut indirect => match r {
            Ok(c) => Ok(c),
            Err(_) => direct.await,
        },
    };
    inner.peers.pierce.remove(&token);
    result.map_err(|e| match e {
        Error::TimedOut | Error::Io(_) => Error::Unreachable(username.to_string()),
        e => e,
    })
}

/// The message channel to `username`, connecting if there is none.
/// Concurrent callers for the same peer share one connection attempt.
pub(crate) async fn link(inner: &Arc<Inner>, username: &str) -> Result<mpsc::Sender<Bytes>> {
    if let Some(l) = inner.peers.links.get(username)
        && !l.tx.is_closed()
    {
        return Ok(l.tx.clone());
    }
    let lock = inner
        .peers
        .connecting
        .entry(username.to_string())
        .or_default()
        .clone();
    let _guard = lock.lock().await;
    if let Some(l) = inner.peers.links.get(username)
        && !l.tx.is_closed()
    {
        return Ok(l.tx.clone());
    }
    let conn = connect(inner, username, ConnKind::Peer).await;
    inner.peers.connecting.remove(username);
    Ok(spawn_peer(inner.clone(), username.to_string(), conn?))
}

pub(crate) async fn send(inner: &Arc<Inner>, username: &str, frame: Bytes) -> Result<()> {
    let tx = link(inner, username).await?;
    tokio::time::timeout(Duration::from_secs(30), tx.send(frame))
        .await
        .map_err(|_| Error::TimedOut)?
        .map_err(|_| Error::Unreachable(username.to_string()))
}

fn spawn_peer(inner: Arc<Inner>, username: String, conn: Conn) -> mpsc::Sender<Bytes> {
    let (mut reader, writer) = conn.split();
    let (tx, rx) = mpsc::channel(256);
    net::spawn_writer(writer, rx);
    let id = inner.peers.next_id.fetch_add(1, Ordering::Relaxed);
    inner
        .peers
        .links
        .insert(username.clone(), Link { id, tx: tx.clone() });
    inner
        .metrics
        .peer_connections
        .store(inner.peers.links.len() as u64, Ordering::Relaxed);
    let reply = tx.clone();
    tokio::spawn(async move {
        loop {
            let frame = match tokio::time::timeout(
                IDLE,
                reader.frame(CodeWidth::U32, MAX_PEER_FRAME),
            )
            .await
            {
                Ok(Ok(Some(f))) => f,
                Ok(Ok(None)) | Err(_) => break,
                Ok(Err(e)) => {
                    tracing::debug!(%username, error = %e, "peer connection failed");
                    break;
                }
            };
            match PeerMessage::decode(frame.code, frame.body) {
                Ok(msg) => on_message(&inner, &username, msg, &reply).await,
                Err(e) => {
                    tracing::debug!(%username, code = frame.code, error = %e, "undecodable peer message")
                }
            }
        }
        inner.peers.links.remove_if(&username, |_, l| l.id == id);
        inner
            .metrics
            .peer_connections
            .store(inner.peers.links.len() as u64, Ordering::Relaxed);
    });
    tx
}

async fn on_message(
    inner: &Arc<Inner>,
    username: &str,
    msg: PeerMessage,
    reply: &mpsc::Sender<Bytes>,
) {
    let respond = |frame: Bytes| {
        let _ = reply.try_send(frame);
    };
    match msg {
        PeerMessage::GetSharedFileList => {
            if inner.is_banned(username) {
                respond(slsk_proto::peer::shared_file_list_frame(&[], &[]));
            } else {
                respond(inner.shares.load().browse_frame());
            }
        }
        PeerMessage::SharedFileList { dirs, .. } => {
            if let Some((_, waiters)) = inner.browse_waiters.remove(username) {
                for w in waiters {
                    let _ = w.send(dirs.clone());
                }
            }
        }
        PeerMessage::SearchResponse(resp) => {
            if let Some(tx) = inner.searches.get(&resp.token).map(|t| t.clone())
                && tx.try_send(resp).is_err()
                && tx.is_closed()
            {
                inner.searches.retain(|_, t| !t.is_closed());
            }
        }
        PeerMessage::UserInfoRequest => {
            let (queued, free) = inner.uploads.queue_summary();
            respond(
                PeerMessage::UserInfoResponse(slsk_proto::peer::UserInfo {
                    description: inner.cfg.description.clone(),
                    picture: None,
                    total_uploads: inner.metrics.uploads_completed.load(Ordering::Relaxed) as u32,
                    queue_size: queued as u32,
                    slots_free: free,
                    upload_permitted: Some(1),
                })
                .encode(),
            );
        }
        PeerMessage::UserInfoResponse(info) => {
            if let Some((_, waiters)) = inner.info_waiters.remove(username) {
                for w in waiters {
                    let _ = w.send(info.clone());
                }
            }
        }
        PeerMessage::FolderContentsRequest { token, folder } => {
            let dirs = if inner.is_banned(username) {
                Vec::new()
            } else {
                inner.shares.load().folder(&folder)
            };
            respond(
                PeerMessage::FolderContentsResponse {
                    token,
                    folder,
                    dirs,
                }
                .encode(),
            );
        }
        PeerMessage::FolderContentsResponse { token, dirs, .. } => {
            if let Some((_, tx)) = inner.folder_waiters.remove(&token) {
                let _ = tx.send(dirs);
            }
        }
        PeerMessage::TransferRequest {
            direction: 1,
            token,
            filename,
            size,
        } => {
            let allowed = downloads::on_transfer_request(
                inner,
                username,
                token,
                &filename,
                size.unwrap_or(0),
            );
            let reason = (!allowed).then(|| reason::CANCELLED.to_string());
            respond(
                PeerMessage::TransferResponse {
                    token,
                    allowed,
                    size: None,
                    reason,
                }
                .encode(),
            );
        }
        PeerMessage::TransferRequest {
            token, filename, ..
        } => {
            // A legacy download request. Answer "Queued" and queue it, rather
            // than accepting and letting the peer open the file connection.
            let reason = match uploads::enqueue(inner, username, filename) {
                Ok(()) => reason::QUEUED.to_string(),
                Err(r) => r.to_string(),
            };
            respond(
                PeerMessage::TransferResponse {
                    token,
                    allowed: false,
                    size: None,
                    reason: Some(reason),
                }
                .encode(),
            );
        }
        PeerMessage::TransferResponse {
            token,
            allowed,
            reason,
            ..
        } => inner.uploads.on_response(token, allowed, reason),
        PeerMessage::QueueUpload { filename } => {
            if let Err(r) = uploads::enqueue(inner, username, filename.clone()) {
                respond(
                    PeerMessage::UploadDenied {
                        filename,
                        reason: r.to_string(),
                    }
                    .encode(),
                );
            }
        }
        PeerMessage::PlaceInQueueRequest { filename } => {
            if let Some(place) = inner.uploads.place(username, &filename) {
                respond(PeerMessage::PlaceInQueueResponse { filename, place }.encode());
            }
        }
        PeerMessage::PlaceInQueueResponse { filename, place } => {
            inner.downloads.set_place(username, &filename, place)
        }
        PeerMessage::UploadFailed { filename } => {
            downloads::on_upload_failed(inner, username, &filename)
        }
        PeerMessage::UploadDenied { filename, reason } => {
            downloads::on_denied(inner, username, &filename, reason)
        }
        PeerMessage::Unknown { code, .. } => {
            tracing::trace!(%username, code, "unknown peer message")
        }
    }
}

pub(crate) async fn listen(inner: Arc<Inner>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                net::keepalive(&stream);
                tokio::spawn(accept(inner.clone(), stream, addr));
            }
            Err(e) => {
                // Out of file descriptors, usually. Back off rather than spin.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn accept(inner: Arc<Inner>, stream: TcpStream, addr: SocketAddr) {
    let mut conn = Conn::new(stream);
    let init = match tokio::time::timeout(Duration::from_secs(30), conn.frame(CodeWidth::U8, 4096))
        .await
    {
        Ok(Ok(Some(f))) => PeerInit::decode(f.code, f.body),
        _ => return,
    };
    match init {
        Ok(PeerInit::PeerInit { username, kind, .. }) => route(inner, username, kind, conn),
        Ok(PeerInit::PierceFirewall { token }) => match inner.peers.pierce.remove(&token) {
            Some((_, tx)) => {
                let _ = tx.send(Some(conn));
            }
            None => tracing::trace!(%addr, token, "pierce for an unknown token"),
        },
        Err(e) => tracing::trace!(%addr, error = %e, "bad peer init"),
    }
}

fn route(inner: Arc<Inner>, username: String, kind: ConnKind, conn: Conn) {
    match kind {
        ConnKind::Peer => {
            spawn_peer(inner, username, conn);
        }
        ConnKind::File => {
            tokio::spawn(downloads::on_file_connection(inner, username, conn));
        }
        ConnKind::Distributed => distributed::on_child(inner, username, conn),
    }
}

/// The server says `username` asked for a connection they could not open
/// themselves: connect to them and pierce with their token.
pub(crate) async fn answer_indirect(
    inner: Arc<Inner>,
    username: String,
    kind: ConnKind,
    ip: Ipv4Addr,
    port: u32,
    token: u32,
) {
    let attempt = async {
        let stream = net::connect(
            SocketAddr::V4(SocketAddrV4::new(ip, port as u16)),
            DIRECT_TIMEOUT,
        )
        .await?;
        let mut conn = Conn::new(stream);
        conn.stream
            .write_all(&PeerInit::PierceFirewall { token }.encode())
            .await?;
        Ok::<_, std::io::Error>(conn)
    };
    match attempt.await {
        Ok(conn) => {
            if kind == ConnKind::Distributed {
                // They connected to us as a child would; we initiated only
                // because they could not.
                distributed::on_child(inner, username, conn);
            } else {
                route(inner, username, kind, conn);
            }
        }
        Err(_) => {
            let _ = inner.send_server(ToServer::CantConnectToPeer { token, username });
        }
    }
}

/// A search from the server or the distributed network. Answering costs a
/// connection to the searcher, so answers are bounded and shed under load.
pub(crate) fn respond_to_search(inner: &Arc<Inner>, username: &str, token: u32, query: &str) {
    inner
        .metrics
        .search_requests
        .fetch_add(1, Ordering::Relaxed);
    if username.eq_ignore_ascii_case(&inner.username())
        || inner.is_banned(username)
        || query.trim().len() < 3
    {
        return;
    }
    let files: Vec<_> = {
        let index = inner.shares.load();
        let excluded = inner.excluded.read();
        index
            .search(query, inner.cfg.max_search_results, &excluded)
            .into_iter()
            .map(|f| f.search_entry())
            .collect()
    };
    if files.is_empty() {
        return;
    }
    let Ok(permit) = inner.responders.clone().try_acquire_owned() else {
        inner
            .metrics
            .search_responses_dropped
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (queued, free) = inner.uploads.queue_summary();
    let response = PeerMessage::SearchResponse(SearchResponse {
        username: inner.username(),
        token,
        files,
        slot_free: free,
        avg_speed: inner.uploads.avg_speed(),
        queue_length: queued as u32,
        private_files: Vec::new(),
    });
    let inner = inner.clone();
    let username = username.to_string();
    tokio::spawn(async move {
        let frame = response.encode();
        if send(&inner, &username, frame).await.is_ok() {
            inner
                .metrics
                .search_responses_sent
                .fetch_add(1, Ordering::Relaxed);
        }
        drop(permit);
    });
}
