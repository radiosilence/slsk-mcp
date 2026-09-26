//! The distributed search network.
//!
//! Searches do not go through the server: they travel down a tree of clients,
//! each passing what its parent sends to its own children. Being a good node
//! means adopting a parent, forwarding everything to children promptly, and
//! answering what matches our shares.
//!
//! Forwarding is the hot path. A search frame is reference-counted bytes, so
//! sending it to every child is a refcount bump per child; a child whose
//! queue is full is disconnected rather than buffered for, since a node that
//! stalls stalls its whole subtree.

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use slsk_proto::peer::{DistribMessage, PeerInit};
use slsk_proto::server::{PossibleParent, ToServer};
use slsk_proto::{CodeWidth, ConnKind};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::net::{self, Conn};
use crate::{Inner, peers};

const MAX_DIST_FRAME: usize = 1 << 20;
const CHILD_QUEUE: usize = 1024;

struct Child {
    id: u64,
    tx: mpsc::Sender<Bytes>,
}

pub(crate) struct Dist {
    /// The adopted parent's username and connection id.
    parent: Mutex<Option<(String, u64)>>,
    level: AtomicI32,
    root: Mutex<String>,
    children: DashMap<String, Child>,
    max_children: AtomicUsize,
    ratio: AtomicU32,
    candidates: DashMap<String, u64>,
    next_id: AtomicU64,
}

impl Dist {
    pub fn new(max_children: usize) -> Self {
        Self {
            parent: Mutex::new(None),
            level: AtomicI32::new(0),
            root: Mutex::new(String::new()),
            children: DashMap::new(),
            max_children: AtomicUsize::new(max_children),
            ratio: AtomicU32::new(0),
            candidates: DashMap::new(),
            next_id: AtomicU64::new(0),
        }
    }

    pub fn set_ratio(&self, ratio: u32) {
        self.ratio.store(ratio, Ordering::Relaxed);
    }

    pub fn summary(&self) -> (Option<String>, i32, String, usize) {
        (
            self.parent.lock().as_ref().map(|p| p.0.clone()),
            self.level.load(Ordering::Relaxed),
            self.root.lock().clone(),
            self.children.len(),
        )
    }

    fn position_frames(&self) -> [Bytes; 2] {
        [
            DistribMessage::BranchLevel {
                level: self.level.load(Ordering::Relaxed),
            }
            .encode(),
            DistribMessage::BranchRoot {
                root: self.root.lock().clone(),
            }
            .encode(),
        ]
    }

    fn broadcast(&self, frame: &Bytes) -> usize {
        let mut slow = Vec::new();
        let mut sent = 0;
        for child in self.children.iter() {
            match child.tx.try_send(frame.clone()) {
                Ok(()) => sent += 1,
                Err(_) => slow.push((child.key().clone(), child.id)),
            }
        }
        for (name, id) in slow {
            self.children.remove_if(&name, |_, c| c.id == id);
        }
        sent
    }
}

/// Forget parent and children and tell the server we need a parent again.
/// `announce` is false when the server connection is already gone.
pub(crate) fn reset(inner: &Inner, announce: bool) {
    let own = inner.username();
    inner.dist.parent.lock().take();
    inner.dist.candidates.clear();
    inner.dist.children.clear();
    inner.metrics.children.store(0, Ordering::Relaxed);
    inner.dist.level.store(0, Ordering::Relaxed);
    *inner.dist.root.lock() = own.clone();
    if announce {
        for msg in [
            ToServer::HaveNoParent { no_parent: true },
            ToServer::BranchRoot { root: own },
            ToServer::BranchLevel { level: 0 },
        ] {
            let _ = inner.send_server(msg);
        }
    }
}

pub(crate) fn on_possible_parents(inner: &Arc<Inner>, parents: &[PossibleParent]) {
    if inner.dist.parent.lock().is_some() {
        return;
    }
    for p in parents.iter().take(10) {
        if inner.dist.candidates.contains_key(&p.username) || p.username == inner.username() {
            continue;
        }
        let id = inner.dist.next_id.fetch_add(1, Ordering::Relaxed);
        inner.dist.candidates.insert(p.username.clone(), id);
        let inner = inner.clone();
        let p = p.clone();
        tokio::spawn(async move {
            let addr = SocketAddr::V4(SocketAddrV4::new(p.ip, p.port as u16));
            let conn = async {
                let mut conn = Conn::new(net::connect(addr, Duration::from_secs(8)).await?);
                conn.stream
                    .write_all(
                        &PeerInit::PeerInit {
                            username: inner.username(),
                            kind: ConnKind::Distributed,
                            token: 0,
                        }
                        .encode(),
                    )
                    .await?;
                Ok::<_, std::io::Error>(conn)
            }
            .await;
            // The listed address can be stale or firewalled; fall back to the
            // usual race, which includes asking them to connect to us.
            let conn = match conn {
                Ok(c) => Some(c),
                Err(_) => peers::connect(&inner, &p.username, ConnKind::Distributed)
                    .await
                    .ok(),
            };
            match conn {
                Some(conn) => run_parent(inner, p.username, id, conn).await,
                None => {
                    inner
                        .dist
                        .candidates
                        .remove_if(&p.username, |_, v| *v == id);
                }
            }
        });
    }
}

/// A candidate parent. It becomes our parent if it is the first to send a
/// search after telling us its position; the rest are dropped.
async fn run_parent(inner: Arc<Inner>, username: String, id: u64, conn: Conn) {
    let (mut reader, _writer) = conn.split();
    let mut position: (Option<i32>, Option<String>) = (None, None);
    let is_parent = |inner: &Inner| inner.dist.parent.lock().as_ref().is_some_and(|p| p.1 == id);
    loop {
        let frame = match tokio::time::timeout(
            Duration::from_secs(600),
            reader.frame(CodeWidth::U8, MAX_DIST_FRAME),
        )
        .await
        {
            Ok(Ok(Some(f))) => f,
            _ => break,
        };
        let Ok(msg) = DistribMessage::decode(frame.code, frame.body) else {
            continue;
        };
        match msg {
            DistribMessage::BranchLevel { level } => {
                position.0 = Some(level);
                if level == 0 {
                    position.1 = Some(username.clone());
                }
                if is_parent(&inner) {
                    adopt_position(&inner, level, position.1.clone());
                }
            }
            DistribMessage::BranchRoot { root } => {
                position.1 = Some(root.clone());
                if is_parent(&inner) {
                    adopt_position(
                        &inner,
                        inner.dist.level.load(Ordering::Relaxed) - 1,
                        Some(root),
                    );
                }
            }
            DistribMessage::Search {
                username: searcher,
                token,
                query,
                raw,
            } => {
                if !is_parent(&inner) {
                    let adopted = {
                        let mut parent = inner.dist.parent.lock();
                        if parent.is_none() && position.0.is_some() {
                            *parent = Some((username.clone(), id));
                            true
                        } else {
                            false
                        }
                    };
                    if !adopted {
                        break;
                    }
                    tracing::info!(parent = %username, level = ?position.0, "adopted distributed parent");
                    inner.dist.candidates.clear();
                    let _ = inner.send_server(ToServer::HaveNoParent { no_parent: false });
                    adopt_position(&inner, position.0.unwrap_or(0), position.1.clone());
                }
                forward(&inner, &raw);
                peers::respond_to_search(&inner, &searcher, token, &query);
            }
            DistribMessage::Embedded { code: 3, payload } if is_parent(&inner) => {
                handle_search_body(&inner, payload)
            }
            _ => {}
        }
    }
    inner.dist.candidates.remove_if(&username, |_, v| *v == id);
    let lost = {
        let mut parent = inner.dist.parent.lock();
        if parent.as_ref().is_some_and(|p| p.1 == id) {
            parent.take();
            true
        } else {
            false
        }
    };
    if lost {
        tracing::info!(parent = %username, "lost distributed parent");
        reset(&inner, true);
    }
}

/// Our position is our parent's plus one.
fn adopt_position(inner: &Inner, parent_level: i32, root: Option<String>) {
    let level = parent_level + 1;
    inner.dist.level.store(level, Ordering::Relaxed);
    let _ = inner.send_server(ToServer::BranchLevel {
        level: level.max(0) as u32,
    });
    if let Some(root) = root {
        *inner.dist.root.lock() = root.clone();
        let _ = inner.send_server(ToServer::BranchRoot { root });
    }
    for frame in inner.dist.position_frames() {
        inner.dist.broadcast(&frame);
    }
}

/// The server sent a search directly: we are a branch root.
pub(crate) fn on_root_search(inner: &Arc<Inner>, payload: Bytes) {
    if inner.dist.parent.lock().is_none() && inner.dist.level.load(Ordering::Relaxed) != 0 {
        inner.dist.level.store(0, Ordering::Relaxed);
        *inner.dist.root.lock() = inner.username();
    }
    handle_search_body(inner, payload);
}

fn handle_search_body(inner: &Arc<Inner>, payload: Bytes) {
    if let Ok(DistribMessage::Search {
        username,
        token,
        query,
        raw,
    }) = DistribMessage::decode(3, payload)
    {
        forward(inner, &raw);
        peers::respond_to_search(inner, &username, token, &query);
    }
}

fn forward(inner: &Inner, raw: &Bytes) {
    if inner.dist.children.is_empty() {
        return;
    }
    let frame = DistribMessage::search_frame(raw);
    let sent = inner.dist.broadcast(&frame);
    inner
        .metrics
        .distributed_forwarded
        .fetch_add(sent as u64, Ordering::Relaxed);
    inner
        .metrics
        .children
        .store(inner.dist.children.len() as u64, Ordering::Relaxed);
}

/// A peer wants us as its parent.
pub(crate) fn on_child(inner: Arc<Inner>, username: String, conn: Conn) {
    if !inner.cfg.accept_children
        || inner.dist.children.len() >= inner.dist.max_children.load(Ordering::Relaxed)
    {
        return;
    }
    let (mut reader, writer) = conn.split();
    let (tx, rx) = mpsc::channel(CHILD_QUEUE);
    for frame in inner.dist.position_frames() {
        let _ = tx.try_send(frame);
    }
    net::spawn_writer(writer, rx);
    let id = inner.dist.next_id.fetch_add(1, Ordering::Relaxed);
    inner
        .dist
        .children
        .insert(username.clone(), Child { id, tx });
    inner
        .metrics
        .children
        .store(inner.dist.children.len() as u64, Ordering::Relaxed);
    tokio::spawn(async move {
        // Children only send pings and obsolete depth reports. Reading keeps
        // the socket drained and tells us when they leave.
        while let Ok(Some(_)) = reader.frame(CodeWidth::U8, MAX_DIST_FRAME).await {}
        inner.dist.children.remove_if(&username, |_, c| c.id == id);
        inner
            .metrics
            .children
            .store(inner.dist.children.len() as u64, Ordering::Relaxed);
    });
}
