//! Downloads: ask the peer to queue the file, wait for them to offer it,
//! receive it on a file connection, resume from a `.part` if one exists.
//!
//! Each download is a small task that sleeps until something happens to it,
//! and progress is atomics read by whoever wants a snapshot, so ten thousand
//! queued downloads cost ten thousand parked tasks and nothing per tick.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use slsk_proto::RawStr;
use slsk_proto::peer::{PeerMessage, file_offset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::net::{Conn, READ_CHUNK};
use crate::{Direction, Error, Inner, Key, TransferView, peers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DownloadState {
    /// Waiting to ask the peer (or to ask again after a failure).
    Queued = 0,
    /// Queued on the peer's side; they will offer it when a slot frees.
    Remote = 1,
    /// The peer offered it; waiting for their file connection.
    Starting = 2,
    Transferring = 3,
    Completed = 4,
    Failed = 5,
    Cancelled = 6,
}

impl DownloadState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Queued,
            1 => Self::Remote,
            2 => Self::Starting,
            3 => Self::Transferring,
            4 => Self::Completed,
            5 => Self::Failed,
            _ => Self::Cancelled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Remote => "remote_queued",
            Self::Starting => "starting",
            Self::Transferring => "transferring",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Failed attempts before a download gives up: offline peers, dropped
/// connections, files the peer offered and never sent.
const MAX_ATTEMPTS: u32 = 8;
/// How long a peer that offered a file has to open the connection for it.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// The largest file accepted from a peer, whatever it announces.
const MAX_FILE_BYTES: u64 = 8 << 30;

pub(crate) struct Download {
    id: u64,
    username: String,
    filename: RawStr,
    dest: PathBuf,
    size: AtomicU64,
    bytes: AtomicU64,
    speed: AtomicU64,
    place: AtomicU32,
    state: AtomicU8,
    attempts: AtomicU32,
    error: Mutex<Option<String>>,
    wake: Notify,
    /// Wakes a receive blocked in a read whenever the state changes.
    stop: Notify,
    /// The token of the peer's current offer, held while `Starting`.
    token: Mutex<Option<u32>>,
    /// Whether a `drive` task owns this download.
    driving: Mutex<bool>,
    /// Held while a file connection writes the `.part`.
    receiving: tokio::sync::Mutex<()>,
}

impl Download {
    fn state(&self) -> DownloadState {
        DownloadState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Move `from` → `to` only if nothing else moved it first.
    fn transition(&self, from: DownloadState, to: DownloadState) -> bool {
        let ok = self
            .state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if ok {
            self.wake.notify_one();
            self.stop.notify_waiters();
        }
        ok
    }

    fn view(&self) -> TransferView {
        let state = self.state();
        TransferView {
            id: self.id,
            direction: Direction::Download,
            username: self.username.clone(),
            filename: self.filename.clone(),
            size: self.size.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            speed: if state == DownloadState::Transferring {
                self.speed.load(Ordering::Relaxed)
            } else {
                0
            },
            state: state.as_str(),
            place: match self.place.load(Ordering::Relaxed) {
                0 => None,
                p => Some(p),
            },
            error: self.error.lock().clone(),
            path: Some(self.dest.clone()),
        }
    }

    fn part(&self) -> PathBuf {
        let mut name = self.dest.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        self.dest.with_file_name(name)
    }
}

#[derive(Default)]
pub(crate) struct Downloads {
    map: DashMap<Key, Arc<Download>>,
    by_token: DashMap<u32, Arc<Download>>,
    next_id: AtomicU64,
}

fn key(username: &str, filename: &RawStr) -> Key {
    (username.to_string(), filename.0.clone())
}

impl Downloads {
    fn get(&self, username: &str, filename: &RawStr) -> Option<Arc<Download>> {
        self.map.get(&key(username, filename)).map(|d| d.clone())
    }

    pub fn views(&self) -> Vec<TransferView> {
        let mut v: Vec<_> = self.map.iter().map(|d| d.view()).collect();
        v.sort_unstable_by_key(|t| t.id);
        v
    }

    pub fn view(&self, username: &str, filename: &RawStr) -> Option<TransferView> {
        self.get(username, filename).map(|d| d.view())
    }

    pub fn cancel(&self, username: &str, filename: &RawStr) -> bool {
        self.get(username, filename)
            .is_some_and(|d| self.cancel_download(&d))
    }

    pub fn remove(&self, username: &str, filename: &RawStr) -> bool {
        match self.map.remove(&key(username, filename)) {
            Some((_, d)) => {
                self.cancel_download(&d);
                true
            }
            None => false,
        }
    }

    fn cancel_download(&self, d: &Download) -> bool {
        let mut from = d.state();
        while !from.is_terminal() {
            if self.leave(d, from, DownloadState::Cancelled) {
                return true;
            }
            from = d.state();
        }
        false
    }

    /// Move `from` → `to`, and drop the route from the current offer's
    /// token to this download. The token lock is held across both, as in
    /// [`on_transfer_request`], so an offer landing meanwhile either fails
    /// its own transition or keeps its token.
    fn leave(&self, d: &Download, from: DownloadState, to: DownloadState) -> bool {
        let mut token = d.token.lock();
        if !d.transition(from, to) {
            return false;
        }
        if let Some(t) = token.take() {
            self.by_token
                .remove_if(&t, |_, x| std::ptr::eq(Arc::as_ptr(x), d));
        }
        true
    }

    pub fn set_place(&self, username: &str, filename: &RawStr, place: u32) {
        if let Some(d) = self.get(username, filename) {
            d.place.store(place, Ordering::Relaxed);
        }
    }
}

pub(crate) fn add(
    inner: &Arc<Inner>,
    username: &str,
    filename: RawStr,
    size: u64,
    dest: PathBuf,
) -> u64 {
    let k = key(username, &filename);
    if let Some(existing) = inner.downloads.map.get(&k)
        && !existing.state().is_terminal()
    {
        return existing.id;
    }
    let d = Arc::new(Download {
        id: inner.downloads.next_id.fetch_add(1, Ordering::Relaxed),
        username: username.to_string(),
        filename,
        dest,
        size: AtomicU64::new(size),
        bytes: AtomicU64::new(0),
        speed: AtomicU64::new(0),
        place: AtomicU32::new(0),
        state: AtomicU8::new(DownloadState::Queued as u8),
        attempts: AtomicU32::new(0),
        error: Mutex::new(None),
        wake: Notify::new(),
        stop: Notify::new(),
        token: Mutex::new(None),
        driving: Mutex::new(false),
        receiving: tokio::sync::Mutex::new(()),
    });
    if let Ok(meta) = std::fs::metadata(d.part()) {
        d.bytes.store(meta.len(), Ordering::Relaxed);
    }
    let id = d.id;
    inner.downloads.map.insert(k, d.clone());
    tokio::spawn(drive(inner.clone(), d));
    id
}

pub(crate) fn retry(inner: &Arc<Inner>, username: &str, filename: &RawStr) -> bool {
    let Some(d) = inner.downloads.get(username, filename) else {
        return false;
    };
    let state = d.state();
    if !matches!(state, DownloadState::Failed | DownloadState::Cancelled) {
        return false;
    }
    d.attempts.store(0, Ordering::Relaxed);
    *d.error.lock() = None;
    if d.transition(state, DownloadState::Queued) {
        tokio::spawn(drive(inner.clone(), d));
    }
    true
}

/// Count a failed attempt from `from`: back to `Queued`, or `Failed` once
/// they are spent. Nothing happens if something else moved it first.
fn attempt_failed(inner: &Inner, d: &Download, from: DownloadState, error: String) -> u32 {
    if d.state() != from {
        return 0;
    }
    let attempts = d.attempts.fetch_add(1, Ordering::Relaxed) + 1;
    if attempts >= MAX_ATTEMPTS {
        fail(inner, d, from, error);
    } else {
        *d.error.lock() = Some(error);
        if from != DownloadState::Queued {
            inner.downloads.leave(d, from, DownloadState::Queued);
        }
    }
    attempts
}

fn fail(inner: &Inner, d: &Download, from: DownloadState, error: String) {
    if inner.downloads.leave(d, from, DownloadState::Failed) {
        *d.error.lock() = Some(error);
        inner
            .metrics
            .downloads_failed
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs until the download reaches a terminal state. At most one runs per
/// download: a retry landing while the last one is still winding down hands
/// the download back to it rather than starting a second.
async fn drive(inner: Arc<Inner>, d: Arc<Download>) {
    {
        let mut driving = d.driving.lock();
        if *driving {
            return;
        }
        *driving = true;
    }
    loop {
        match d.state() {
            DownloadState::Queued => {
                if !inner.logged_in() {
                    let mut status = inner.status.subscribe();
                    let _ = status
                        .wait_for(|s| matches!(s, crate::Status::LoggedIn { .. }))
                        .await;
                    continue;
                }
                let frame = PeerMessage::QueueUpload {
                    filename: d.filename.clone(),
                }
                .encode();
                match peers::send(&inner, &d.username, frame).await {
                    Ok(()) => {
                        d.transition(DownloadState::Queued, DownloadState::Remote);
                    }
                    Err(e) => {
                        let attempts =
                            attempt_failed(&inner, &d, DownloadState::Queued, e.to_string());
                        if d.state() != DownloadState::Queued {
                            continue;
                        }
                        let wait = Duration::from_secs(30 * u64::from(attempts))
                            .min(Duration::from_secs(900));
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = d.wake.notified() => {}
                        }
                    }
                }
            }
            DownloadState::Remote => {
                // Ask where we are in their queue now and then; the answer is
                // informational, and a peer that went away shows up as a send
                // failure here.
                tokio::select! {
                    _ = d.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {
                        let frame = PeerMessage::PlaceInQueueRequest { filename: d.filename.clone() }.encode();
                        // Gone, or restarted and holding nothing for us: ask
                        // again from the top.
                        if peers::send(&inner, &d.username, frame).await.is_err() {
                            d.transition(DownloadState::Remote, DownloadState::Queued);
                        }
                    }
                }
            }
            DownloadState::Starting => {
                tokio::select! {
                    _ = d.wake.notified() => {}
                    _ = tokio::time::sleep(START_TIMEOUT) => {
                        attempt_failed(&inner, &d, DownloadState::Starting, "offered and never sent".into());
                    }
                }
            }
            DownloadState::Transferring => d.wake.notified().await,
            _ => {
                let mut driving = d.driving.lock();
                if d.state().is_terminal() {
                    *driving = false;
                    return;
                }
            }
        }
    }
}

/// The peer offers the file. Accept it if we asked for it.
pub(crate) fn on_transfer_request(
    inner: &Inner,
    username: &str,
    token: u32,
    filename: &RawStr,
    size: u64,
) -> bool {
    let Some(d) = inner.downloads.get(username, filename) else {
        return false;
    };
    let from = d.state();
    if !matches!(
        from,
        DownloadState::Queued | DownloadState::Remote | DownloadState::Starting
    ) {
        return false;
    }
    // The size is what `receive` writes up to, so a peer does not get to
    // choose it: an offer that disagrees with what was queued, or that is
    // larger than any album track, fails the download.
    let queued = d.size.load(Ordering::Relaxed);
    if size > 0 && queued > 0 && size != queued {
        fail(
            inner,
            &d,
            from,
            format!("the peer offered {size} bytes, not the {queued} queued"),
        );
        return false;
    }
    let size = if size > 0 { size } else { queued };
    if size > MAX_FILE_BYTES {
        fail(
            inner,
            &d,
            from,
            format!("the peer offered {size} bytes, more than the {MAX_FILE_BYTES} allowed"),
        );
        return false;
    }
    // Held across the transition, so a cancel that lands meanwhile waits to
    // forget this token rather than missing it.
    let mut current = d.token.lock();
    if !d.transition(from, DownloadState::Starting) {
        return false;
    }
    d.size.store(size, Ordering::Relaxed);
    d.place.store(0, Ordering::Relaxed);
    if let Some(old) = current.replace(token) {
        inner
            .downloads
            .by_token
            .remove_if(&old, |_, t| Arc::ptr_eq(t, &d));
    }
    inner.downloads.by_token.insert(token, d.clone());
    true
}

pub(crate) fn on_upload_failed(inner: &Inner, username: &str, filename: &RawStr) {
    if let Some(d) = inner.downloads.get(username, filename) {
        // Not while transferring: the receiver still holds the `.part` file,
        // and its own error, when the connection ends, re-queues it.
        // Counted like any other attempt: a peer that fails the same file
        // every time it reaches it would otherwise be asked forever.
        let from = d.state();
        if matches!(from, DownloadState::Starting | DownloadState::Remote) {
            attempt_failed(inner, &d, from, "the peer failed to send it".into());
        }
    }
}

pub(crate) fn on_denied(inner: &Inner, username: &str, filename: &RawStr, reason: String) {
    if let Some(d) = inner.downloads.get(username, filename) {
        let from = d.state();
        if !from.is_terminal() {
            fail(inner, &d, from, reason);
        }
    }
}

/// An incoming file connection. The uploader sends the transfer token first;
/// we answer with how much we already have.
pub(crate) async fn on_file_connection(inner: Arc<Inner>, username: String, mut conn: Conn) {
    let token = match tokio::time::timeout(Duration::from_secs(60), conn.u32()).await {
        Ok(Ok(t)) => t,
        _ => return,
    };
    // A token is the uploader's to use: another peer naming it is not
    // let into someone else's transfer.
    let Some((_, d)) = inner
        .downloads
        .by_token
        .remove_if(&token, |_, d| d.username == username)
    else {
        tracing::debug!(%username, token, "file connection for an unknown transfer");
        return;
    };
    d.token.lock().take_if(|t| *t == token);
    // A receive from before a cancel and retry may still be winding down.
    // A state change wakes it out of its read, so this is a short wait;
    // bounded anyway, well inside START_TIMEOUT, and two writers to one
    // `.part` would interleave.
    let Ok(_receiving) = tokio::time::timeout(Duration::from_secs(10), d.receiving.lock()).await
    else {
        tracing::debug!(%username, file = %d.filename, "the previous receive is still running");
        return;
    };
    // A partial file larger than the offer is not from this file, and no
    // attempt can succeed while it is there.
    let part = tokio::fs::metadata(d.part()).await.map_or(0, |m| m.len());
    let size = d.size.load(Ordering::Relaxed);
    if part > size {
        fail(
            &inner,
            &d,
            DownloadState::Starting,
            format!(
                "the partial file {} holds {part} bytes, more than the {size} offered",
                d.part().display()
            ),
        );
        return;
    }
    if !d.transition(DownloadState::Starting, DownloadState::Transferring) {
        return;
    }
    inner
        .metrics
        .downloads_active
        .fetch_add(1, Ordering::Relaxed);
    let result = receive(&inner, &d, conn).await;
    inner
        .metrics
        .downloads_active
        .fetch_sub(1, Ordering::Relaxed);
    match result {
        Ok(()) => {
            if d.transition(DownloadState::Transferring, DownloadState::Completed) {
                *d.error.lock() = None;
                inner
                    .metrics
                    .downloads_completed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) if d.state() != DownloadState::Transferring => {
            tracing::debug!(error = %e, state = d.state().as_str(), "download stopped")
        }
        Err(e) => {
            attempt_failed(&inner, &d, DownloadState::Transferring, e.to_string());
        }
    }
}

/// How many bytes of a resumed file's start are compared with what the peer
/// sends first.
const RESUME_PROBE: u64 = 4096;

/// Whether a peer asked to resume sent the file's start instead: its first
/// bytes are the ones already at the start of the partial file.
fn restarted(head: &[u8], first: &[u8]) -> bool {
    !head.is_empty() && first.len() >= head.len() && first[..head.len()] == *head
}

async fn receive(inner: &Inner, d: &Download, mut conn: Conn) -> Result<(), Error> {
    let size = d.size.load(Ordering::Relaxed);
    let part = d.part();
    if let Some(dir) = part.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .await?;
    let mut offset = file.metadata().await?.len();
    if offset > size {
        return Err(std::io::Error::other(format!(
            "the partial file holds {offset} bytes, more than the {size} offered"
        ))
        .into());
    }
    d.bytes.store(offset, Ordering::Relaxed);
    conn.stream.write_all(&file_offset(offset)).await?;

    // What the peer sent before the loop below: the bytes already buffered
    // with the token, and on a resume enough more to tell where it started.
    let mut first: Vec<u8> = conn.buf.split().to_vec();
    if offset > 0 {
        // Asked to resume, a peer should send from `offset`. Some send the
        // whole file from the start regardless, and appending that to what is
        // already here splices two copies into one file that no longer
        // decodes. Such a peer's first bytes are the file's own first bytes,
        // which a continuation almost never is (every audio format opens with
        // a header), so compare them and start over if they match.
        let probe = offset.min(RESUME_PROBE) as usize;
        let mut head = vec![0u8; probe];
        tokio::fs::File::open(&part)
            .await?
            .read_exact(&mut head)
            .await?;
        while first.len() < probe {
            let mut more = vec![0u8; probe - first.len()];
            let n = tokio::time::timeout(Duration::from_secs(120), conn.stream.read(&mut more))
                .await
                .map_err(|_| Error::TimedOut)??;
            if n == 0 {
                break;
            }
            first.extend_from_slice(&more[..n]);
        }
        if restarted(&head, &first) {
            tracing::warn!(
                username = %d.username,
                file = %d.filename,
                offset,
                "the peer ignored the resume offset and sent from the start; starting the file again"
            );
            file.set_len(0).await?;
            offset = 0;
            d.bytes.store(0, Ordering::Relaxed);
        }
    }
    let mut file = tokio::io::BufWriter::with_capacity(READ_CHUNK, file);
    let leftover: Bytes = first.into();
    let mut window = (Instant::now(), offset);
    let mut take = |n: usize, offset: &mut u64| {
        *offset += n as u64;
        d.bytes.store(*offset, Ordering::Relaxed);
        inner
            .metrics
            .bytes_downloaded
            .fetch_add(n as u64, Ordering::Relaxed);
        let elapsed = window.0.elapsed();
        if elapsed >= Duration::from_secs(1) {
            d.speed.store(
                ((*offset - window.1) as f64 / elapsed.as_secs_f64()) as u64,
                Ordering::Relaxed,
            );
            window = (Instant::now(), *offset);
        }
    };
    if !leftover.is_empty() {
        let n = leftover.len().min((size - offset) as usize);
        file.write_all(&leftover[..n]).await?;
        take(n, &mut offset);
    }
    let mut buf = vec![0u8; READ_CHUNK];
    while offset < size {
        let stopped = d.stop.notified();
        tokio::pin!(stopped);
        stopped.as_mut().enable();
        if d.state() != DownloadState::Transferring {
            file.flush().await?;
            return Err(std::io::Error::other("cancelled").into());
        }
        let want = buf.len().min((size - offset) as usize);
        let read =
            tokio::time::timeout(Duration::from_secs(120), conn.stream.read(&mut buf[..want]));
        let n = tokio::select! {
            n = read => n.map_err(|_| Error::TimedOut)??,
            _ = stopped => continue,
        };
        if n == 0 {
            file.flush().await?;
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        inner.down_limit.take(n).await;
        file.write_all(&buf[..n]).await?;
        take(n, &mut offset);
    }
    file.flush().await?;
    file.get_ref().sync_all().await?;
    drop(file);
    // Cancelled or retried while the last bytes arrived: the `.part` stays.
    if d.state() != DownloadState::Transferring {
        return Err(std::io::Error::other("cancelled").into());
    }
    tokio::fs::rename(&part, &d.dest).await?;
    // The downloader closes a finished transfer; the uploader must not.
    let _ = conn.stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod resume_tests {
    use super::restarted;

    #[test]
    fn a_peer_that_starts_over_is_caught() {
        let file = b"fLaC\x00\x00\x00\x22 the rest of a flac file, and its audio";
        let head = &file[..16];
        // Resumed properly: the bytes after the partial.
        assert!(!restarted(head, &file[16..]));
        // Sent from the start again.
        assert!(restarted(head, file));
        // Too little arrived to tell: not assumed to be a restart.
        assert!(!restarted(head, &file[..8]));
    }
}

#[cfg(test)]
mod offer_tests {
    use super::*;

    #[tokio::test]
    async fn an_offer_of_another_size_fails_the_download() {
        let (e, tmp) = crate::offline_engine().await;
        let name: RawStr = "m\\a.flac".into();
        add(&e.0, "peer", name.clone(), 1000, tmp.path().join("a.flac"));
        assert!(!on_transfer_request(&e.0, "peer", 1, &name, 1001));
        let view = e.0.downloads.view("peer", &name).unwrap();
        assert_eq!(view.state, "failed", "{view:?}");
        assert!(e.0.downloads.by_token.is_empty());
    }

    #[tokio::test]
    async fn an_offer_past_the_cap_fails_the_download() {
        let (e, tmp) = crate::offline_engine().await;
        let name: RawStr = "m\\a.flac".into();
        add(&e.0, "peer", name.clone(), 0, tmp.path().join("a.flac"));
        assert!(!on_transfer_request(
            &e.0,
            "peer",
            1,
            &name,
            MAX_FILE_BYTES + 1
        ));
        assert_eq!(e.0.downloads.view("peer", &name).unwrap().state, "failed");
    }

    #[tokio::test]
    async fn offers_leave_no_tokens_behind() {
        let (e, tmp) = crate::offline_engine().await;
        let inner = &e.0;
        let name: RawStr = "m\\a.flac".into();
        add(inner, "peer", name.clone(), 1000, tmp.path().join("a.flac"));
        let tokens = || {
            let mut t: Vec<u32> = inner.downloads.by_token.iter().map(|e| *e.key()).collect();
            t.sort_unstable();
            t
        };

        assert!(on_transfer_request(inner, "peer", 1, &name, 1000));
        assert!(on_transfer_request(inner, "peer", 2, &name, 1000));
        assert_eq!(tokens(), [2], "a second offer replaces the first");

        on_upload_failed(inner, "peer", &name);
        assert_eq!(inner.downloads.view("peer", &name).unwrap().state, "queued");
        assert!(tokens().is_empty());

        assert!(on_transfer_request(inner, "peer", 3, &name, 1000));
        assert!(inner.downloads.cancel("peer", &name));
        assert!(tokens().is_empty());

        assert!(retry(inner, "peer", &name));
        assert!(on_transfer_request(inner, "peer", 4, &name, 1000));
        assert!(inner.downloads.remove("peer", &name));
        assert!(tokens().is_empty());
    }

    #[tokio::test]
    async fn a_re_offer_leaves_another_peers_token_alone() {
        let (e, tmp) = crate::offline_engine().await;
        let inner = &e.0;
        let (a, b): (RawStr, RawStr) = ("m\\a.flac".into(), "m\\b.flac".into());
        add(inner, "one", a.clone(), 10, tmp.path().join("a.flac"));
        add(inner, "two", b.clone(), 10, tmp.path().join("b.flac"));
        assert!(on_transfer_request(inner, "one", 5, &a, 10));
        // Peers choose their own tokens, so two can collide.
        assert!(on_transfer_request(inner, "two", 5, &b, 10));
        assert!(on_transfer_request(inner, "one", 6, &a, 10));
        assert_eq!(inner.downloads.by_token.get(&5).unwrap().username, "two");
    }

    #[tokio::test]
    async fn a_receive_cancelled_at_the_end_keeps_the_part() {
        let (e, tmp) = crate::offline_engine().await;
        let inner = &e.0;
        let name: RawStr = "m\\a.flac".into();
        let dest = tmp.path().join("a.flac");
        add(inner, "peer", name.clone(), 0, dest.clone());
        let d = inner.downloads.get("peer", &name).unwrap();
        d.state
            .store(DownloadState::Cancelled as u8, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _peer = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (ours, _) = listener.accept().await.unwrap();
        assert!(receive(inner, &d, Conn::new(ours)).await.is_err());
        assert!(!dest.exists());
        assert!(d.part().exists());
    }
}
