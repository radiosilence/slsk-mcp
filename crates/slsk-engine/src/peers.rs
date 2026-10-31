//! Peer connections: making them, accepting them, and what arrives on them.
//!
//! An outgoing connection races both routes the modern protocol allows at
//! once — a direct connect to the address the server gives, and an indirect
//! request asking the peer to connect back and pierce our firewall — and
//! keeps whichever lands first. Doing them in sequence, as older clients do,
//! costs a full timeout for every firewalled peer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use slsk_proto::peer::{PeerInit, PeerMessage, SearchResponse};
use slsk_proto::server::ToServer;
use slsk_proto::{CodeWidth, ConnKind, reason};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::net::{self, Conn};
use crate::{Defer, Error, Inner, Result, Status, distributed, downloads, uploads};

/// A peer connection that carries nothing for this long is closed. Peers
/// reconnect on demand, and an idle socket is a file descriptor and two
/// tasks for nothing.
const IDLE: Duration = Duration::from_secs(600);
/// Once a message has begun to arrive, the whole of it must arrive within
/// this; a browse gives up waiting after as long.
const FRAME_DEADLINE: Duration = Duration::from_secs(180);
/// Every peer message but a share list fits in this compressed.
const MAX_PEER_MESSAGE: usize = 1 << 20;
/// Share lists of very large collections are tens of megabytes compressed.
/// One is buffered only while a browse of that peer is waiting for it.
const MAX_SHARE_LIST: usize = 128 << 20;
/// Messages larger than this are decoded off the async workers: inflating
/// and parsing a big share list takes long enough to stall them.
const INLINE_DECODE: usize = 64 << 10;
const DIRECT_TIMEOUT: Duration = Duration::from_secs(8);
const INDIRECT_TIMEOUT: Duration = Duration::from_secs(30);
const ADDRESS_TTL: Duration = Duration::from_secs(600);

pub(crate) struct Link {
    id: u64,
    ip: Option<IpAddr>,
    /// Set for a connection we opened, or an inbound one whose address the
    /// server has confirmed belongs to the user it named.
    trusted: Arc<AtomicBool>,
    pub tx: mpsc::Sender<Bytes>,
}

#[derive(Default)]
pub(crate) struct Peers {
    pub links: DashMap<String, Link>,
    connecting: DashMap<String, Arc<Mutex<()>>>,
    addresses: DashMap<String, (SocketAddrV4, Instant)>,
    address_waiters: DashMap<String, Vec<oneshot::Sender<SocketAddrV4>>>,
    pierce: DashMap<u32, oneshot::Sender<Option<Conn>>>,
    /// Indirect requests we stopped waiting for because the direct route won.
    /// The peer does not know that and pierces anyway; a pierce that arrives
    /// for one of these is adopted rather than closed, because closing it
    /// kills a socket the peer has already started sending on.
    settled: DashMap<u32, (String, ConnKind, Instant)>,
    next_id: AtomicU64,
    inbound: AtomicUsize,
    inbound_by_ip: DashMap<IpAddr, usize>,
}

/// One inbound connection's place under the limits on how many strangers
/// may hold open. Released on drop.
pub(crate) struct Inbound {
    inner: Arc<Inner>,
    ip: IpAddr,
}

impl Inbound {
    fn admit(inner: &Arc<Inner>, ip: IpAddr) -> Option<Self> {
        let peers = &inner.peers;
        if peers.inbound.fetch_add(1, Ordering::Relaxed) >= inner.cfg.max_inbound_connections {
            peers.inbound.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        let mut n = peers.inbound_by_ip.entry(ip).or_default();
        if *n >= inner.cfg.max_inbound_per_address {
            drop(n);
            peers.inbound.fetch_sub(1, Ordering::Relaxed);
            peers.inbound_by_ip.remove_if(&ip, |_, n| *n == 0);
            return None;
        }
        *n += 1;
        Some(Self {
            inner: inner.clone(),
            ip,
        })
    }
}

impl Drop for Inbound {
    fn drop(&mut self) {
        let peers = &self.inner.peers;
        peers.inbound.fetch_sub(1, Ordering::Relaxed);
        if let Some(mut n) = peers.inbound_by_ip.get_mut(&self.ip) {
            *n -= 1;
        }
        peers.inbound_by_ip.remove_if(&self.ip, |_, n| *n == 0);
    }
}

pub(crate) fn on_address(inner: &Inner, username: &str, ip: Ipv4Addr, port: u32) {
    let addr = SocketAddrV4::new(ip, port as u16);
    if port != 0 && !ip.is_unspecified() {
        let addresses = &inner.peers.addresses;
        // Every peer ever asked about otherwise stays; past a few thousand,
        // the stale go. If they are all fresh it is only a cache, and
        // emptying it keeps each insert from walking every entry again.
        if addresses.len() >= 4096 {
            addresses.retain(|_, (_, at)| at.elapsed() < ADDRESS_TTL);
            if addresses.len() >= 4096 {
                addresses.clear();
            }
        }
        addresses.insert(username.to_string(), (addr, Instant::now()));
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
    let _forget = Defer::new(|| crate::forget_closed(&inner.peers.address_waiters, username));
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
    // A pierce that arrives after this attempt is over, whether it finished
    // or its caller gave up on it, is adopted rather than left waiting.
    let _settle = Defer::new(|| {
        if inner.peers.pierce.remove(&token).is_some() && indirect_requested {
            let settled = &inner.peers.settled;
            settled.retain(|_, v| v.2.elapsed() < INDIRECT_TIMEOUT * 2);
            settled.insert(token, (username.to_string(), kind, Instant::now()));
        }
    });

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
        && l.trusted.load(Ordering::Relaxed)
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
    // Only the holder clears the entry, and only its own, so a waiter giving
    // up cannot let a second attempt start beside this one. Whether it
    // connects or finds a live link, it goes: kept, every peer ever messaged
    // would leave one behind.
    let _forget = Defer::new(|| {
        inner
            .peers
            .connecting
            .remove_if(username, |_, l| Arc::ptr_eq(l, &lock));
    });
    let live = inner
        .peers
        .links
        .get(username)
        .filter(|l| !l.tx.is_closed())
        .map(|l| (l.tx.clone(), l.ip, l.trusted.clone()));
    if let Some((tx, ip, trusted)) = live
        && (trusted.load(Ordering::Relaxed) || verify(inner, username, ip).await)
    {
        trusted.store(true, Ordering::Relaxed);
        return Ok(tx);
    }
    let conn = connect(inner, username, ConnKind::Peer).await?;
    Ok(spawn_peer(inner.clone(), username.to_string(), conn, None))
}

pub(crate) async fn send(inner: &Arc<Inner>, username: &str, frame: Bytes) -> Result<()> {
    let tx = link(inner, username).await?;
    tokio::time::timeout(Duration::from_secs(30), tx.send(frame))
        .await
        .map_err(|_| Error::TimedOut)?
        .map_err(|_| Error::Unreachable(username.to_string()))
}

/// Whether `ip` is where the server says `username` is. A peer that opens a
/// connection names itself, and this is all that makes the name good. A
/// private address passes for a user the server places at our own public
/// address: a peer on our side of the same NAT.
async fn verify(inner: &Inner, username: &str, ip: Option<IpAddr>) -> bool {
    let Some(IpAddr::V4(ip)) = ip else {
        return false;
    };
    let Ok(addr) = address(inner, username).await else {
        return false;
    };
    let known = *addr.ip();
    known == ip
        || ((ip.is_private() || ip.is_loopback() || ip.is_link_local())
            && matches!(&*inner.status.borrow(), Status::LoggedIn { own_ip } if *own_ip == known))
}

/// Messages that answer something we asked of `username`, or act on a
/// transfer with them. A request, or a search response, means nothing more
/// coming from someone else, so only these need the sender verified.
fn answers_us(msg: &PeerMessage) -> bool {
    matches!(
        msg,
        PeerMessage::SharedFileList { .. }
            | PeerMessage::FolderContentsResponse { .. }
            | PeerMessage::UserInfoResponse(_)
            | PeerMessage::TransferRequest { direction: 1, .. }
            | PeerMessage::TransferResponse { .. }
            | PeerMessage::PlaceInQueueResponse { .. }
            | PeerMessage::UploadFailed { .. }
            | PeerMessage::UploadDenied { .. }
    )
}

/// `inbound` is set for a connection a stranger opened and named themselves
/// on. It is not trusted as that user until [`verify`] says so.
fn spawn_peer(
    inner: Arc<Inner>,
    username: String,
    conn: Conn,
    inbound: Option<Inbound>,
) -> mpsc::Sender<Bytes> {
    let ip = conn.stream.peer_addr().ok().map(|a| a.ip().to_canonical());
    let (mut reader, writer) = conn.split();
    let (tx, rx) = mpsc::channel(256);
    drop(net::spawn_writer(writer, rx));
    let id = inner.peers.next_id.fetch_add(1, Ordering::Relaxed);
    let trusted = Arc::new(AtomicBool::new(inbound.is_none()));
    let link = Link {
        id,
        ip,
        trusted: trusted.clone(),
        tx: tx.clone(),
    };
    match inner.peers.links.entry(username.clone()) {
        // An unverified link never displaces a live trusted one.
        Entry::Occupied(e)
            if inbound.is_some()
                && !e.get().tx.is_closed()
                && e.get().trusted.load(Ordering::Relaxed) => {}
        Entry::Occupied(mut e) => {
            e.insert(link);
        }
        Entry::Vacant(e) => {
            e.insert(link);
        }
    }
    inner
        .metrics
        .peer_connections
        .store(inner.peers.links.len() as u64, Ordering::Relaxed);
    let reply = tx.clone();
    tokio::spawn(async move {
        let _inbound = inbound;
        let mut refused = false;
        loop {
            match tokio::time::timeout(IDLE, reader.ready()).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) | Err(_) => break,
                Ok(Err(e)) => {
                    tracing::debug!(%username, error = %e, "peer connection failed");
                    break;
                }
            }
            // A share list's larger bound is only for a link known to be
            // that user's: a stranger naming someone we are browsing would
            // otherwise be let send 128 MiB, and have it inflated, on each
            // of hundreds of connections. A peer may answer a browse on a
            // connection of its own, so while one waits, such a link is
            // checked before its next frame is read.
            if !trusted.load(Ordering::Relaxed) && !refused && browsing(&inner, &username) {
                if verify(&inner, &username, ip).await {
                    trusted.store(true, Ordering::Relaxed);
                } else {
                    refused = true;
                }
            }
            let max = |code| {
                if code == 5 && trusted.load(Ordering::Relaxed) && browsing(&inner, &username) {
                    MAX_SHARE_LIST
                } else {
                    MAX_PEER_MESSAGE
                }
            };
            let frame =
                match tokio::time::timeout(FRAME_DEADLINE, reader.frame_by(CodeWidth::U32, max))
                    .await
                {
                    Ok(Ok(Some(f))) => f,
                    Ok(Ok(None)) | Err(_) => break,
                    Ok(Err(e)) => {
                        tracing::debug!(%username, error = %e, "peer connection failed");
                        break;
                    }
                };
            // A share list is inflated only when we asked for it: unasked
            // for, it is up to 256 MiB of work for nothing.
            // Search results and folder contents inflate too, up to 16 MiB
            // each, and anyone may send them: with nothing waiting for one,
            // it is dropped unread.
            if (frame.code == 5 && !browsing(&inner, &username))
                || (frame.code == 9 && inner.searches.is_empty())
                || (frame.code == 37 && inner.folder_waiters.is_empty())
            {
                continue;
            }
            // A share list inflates to as much as 256 MiB from a megabyte, so
            // the sender is checked first, not after: anyone can open a
            // connection naming the user being browsed.
            if frame.code == 5 && !trusted.load(Ordering::Relaxed) {
                if refused || !verify(&inner, &username, ip).await {
                    refused = true;
                    continue;
                }
                trusted.store(true, Ordering::Relaxed);
            }
            let code = frame.code;
            let decoded = if frame.body.len() > INLINE_DECODE {
                // Each can inflate to 16 MiB or more, and any connection can
                // send one: a few at a time bounds what they hold together.
                let Ok(permit) = inner.decoders.clone().acquire_owned().await else {
                    break;
                };
                match tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    PeerMessage::decode(code, frame.body)
                })
                .await
                {
                    Ok(d) => d,
                    Err(_) => break,
                }
            } else {
                PeerMessage::decode(code, frame.body)
            };
            match decoded {
                Ok(msg) if answers_us(&msg) && !trusted.load(Ordering::Relaxed) => {
                    if !refused && verify(&inner, &username, ip).await {
                        trusted.store(true, Ordering::Relaxed);
                        on_message(&inner, &username, msg, &reply);
                    } else {
                        refused = true;
                        tracing::debug!(%username, ?ip, code, "answer from an unverified peer ignored");
                    }
                }
                Ok(msg) => on_message(&inner, &username, msg, &reply),
                Err(e) => {
                    tracing::debug!(%username, code, error = %e, "undecodable peer message")
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

/// Whether a browse of `username` is waiting for its share list.
fn browsing(inner: &Inner, username: &str) -> bool {
    inner
        .browse_waiters
        .get(username)
        .is_some_and(|w| w.iter().any(|tx| !tx.is_closed()))
}

fn on_message(inner: &Arc<Inner>, username: &str, msg: PeerMessage, reply: &mpsc::Sender<Bytes>) {
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
            let profile = inner.profile.read().clone();
            respond(
                PeerMessage::UserInfoResponse(slsk_proto::peer::UserInfo {
                    description: profile.description,
                    picture: profile.picture,
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
            // A file asked after that we hold nothing for was dropped while
            // the peer was offline, or queued before a restart of ours. The
            // peer still has it queued and would wait for it for good, so it
            // is queued again, with the same checks as a fresh request.
            // Not one we finished, failed or cancelled lately: a poll that
            // crossed the end of an upload, or one cancelled here.
            let mut place = inner.uploads.place(username, &filename);
            if place.is_none() && !inner.uploads.settled(username, &filename) {
                if let Err(r) = uploads::enqueue(inner, username, filename.clone()) {
                    respond(
                        PeerMessage::UploadDenied {
                            filename,
                            reason: r.to_string(),
                        }
                        .encode(),
                    );
                    return;
                }
                place = inner.uploads.place(username, &filename);
            }
            if let Some(place) = place {
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
                let Some(inbound) = Inbound::admit(&inner, addr.ip().to_canonical()) else {
                    tracing::trace!(%addr, "inbound connection refused: too many open");
                    continue;
                };
                net::keepalive(&stream);
                tokio::spawn(accept(inner.clone(), stream, addr, inbound));
            }
            Err(e) => {
                // Out of file descriptors, usually. Back off rather than spin.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn accept(inner: Arc<Inner>, stream: TcpStream, addr: SocketAddr, inbound: Inbound) {
    let mut conn = Conn::new(stream);
    let init = match tokio::time::timeout(Duration::from_secs(30), conn.frame(CodeWidth::U8, 4096))
        .await
    {
        Ok(Ok(Some(f))) => PeerInit::decode(f.code, f.body),
        _ => return,
    };
    match init {
        Ok(PeerInit::PeerInit { username, kind, .. }) => {
            route(inner, username, kind, conn, Some(inbound))
        }
        // A pierce answers a request of ours, so it is no stranger's.
        Ok(PeerInit::PierceFirewall { token }) => match inner.peers.pierce.remove(&token) {
            Some((_, tx)) => {
                let _ = tx.send(Some(conn));
            }
            None => match inner.peers.settled.remove(&token) {
                Some((_, (username, ConnKind::Peer, _))) => {
                    spawn_peer(inner, username, conn, None);
                }
                _ => tracing::trace!(%addr, token, "pierce for an unknown token"),
            },
        },
        Err(e) => tracing::trace!(%addr, error = %e, "bad peer init"),
    }
}

fn route(
    inner: Arc<Inner>,
    username: String,
    kind: ConnKind,
    conn: Conn,
    inbound: Option<Inbound>,
) {
    match kind {
        ConnKind::Peer => {
            spawn_peer(inner, username, conn, inbound);
        }
        ConnKind::File => {
            tokio::spawn(async move {
                downloads::on_file_connection(inner, username, conn).await;
                drop(inbound);
            });
        }
        ConnKind::Distributed => distributed::on_child(inner, username, conn, inbound),
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
        Ok(conn) => route(inner, username, kind, conn, None),
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
    // Shed before searching, so load costs no index work either.
    let Ok(permit) = inner.responders.clone().try_acquire_owned() else {
        inner
            .metrics
            .search_responses_dropped
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
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
