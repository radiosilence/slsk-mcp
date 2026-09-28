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
}

impl Download {
    fn state(&self) -> DownloadState {
        DownloadState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set(&self, s: DownloadState) {
        self.state.store(s as u8, Ordering::Release);
        self.wake.notify_one();
    }

    /// Move `from` → `to` only if nothing else moved it first.
    fn transition(&self, from: DownloadState, to: DownloadState) -> bool {
        let ok = self
            .state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if ok {
            self.wake.notify_one();
        }
        ok
    }

    fn fail(&self, error: impl Into<String>) {
        *self.error.lock() = Some(error.into());
        self.set(DownloadState::Failed);
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
        match self.get(username, filename) {
            Some(d) if !d.state().is_terminal() => {
                d.set(DownloadState::Cancelled);
                true
            }
            _ => false,
        }
    }

    pub fn remove(&self, username: &str, filename: &RawStr) -> bool {
        match self.map.remove(&key(username, filename)) {
            Some((_, d)) => {
                if !d.state().is_terminal() {
                    d.set(DownloadState::Cancelled);
                }
                true
            }
            None => false,
        }
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

async fn drive(inner: Arc<Inner>, d: Arc<Download>) {
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
                        let attempts = d.attempts.fetch_add(1, Ordering::Relaxed) + 1;
                        if attempts >= MAX_ATTEMPTS {
                            inner
                                .metrics
                                .downloads_failed
                                .fetch_add(1, Ordering::Relaxed);
                            d.fail(e.to_string());
                            continue;
                        }
                        *d.error.lock() = Some(e.to_string());
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
                        let _ = peers::send(&inner, &d.username, frame).await;
                    }
                }
            }
            DownloadState::Starting => {
                tokio::select! {
                    _ = d.wake.notified() => {}
                    _ = tokio::time::sleep(START_TIMEOUT) => {
                        d.transition(DownloadState::Starting, DownloadState::Queued);
                    }
                }
            }
            DownloadState::Transferring => d.wake.notified().await,
            _ => return,
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
    if size > 0 {
        d.size.store(size, Ordering::Relaxed);
    }
    inner.downloads.by_token.insert(token, d.clone());
    d.place.store(0, Ordering::Relaxed);
    d.transition(from, DownloadState::Starting)
}

pub(crate) fn on_upload_failed(inner: &Inner, username: &str, filename: &RawStr) {
    if let Some(d) = inner.downloads.get(username, filename) {
        for from in [
            DownloadState::Starting,
            DownloadState::Transferring,
            DownloadState::Remote,
        ] {
            if d.transition(from, DownloadState::Queued) {
                break;
            }
        }
    }
}

pub(crate) fn on_denied(inner: &Inner, username: &str, filename: &RawStr, reason: String) {
    if let Some(d) = inner.downloads.get(username, filename)
        && !d.state().is_terminal()
    {
        inner
            .metrics
            .downloads_failed
            .fetch_add(1, Ordering::Relaxed);
        d.fail(reason);
    }
}

/// An incoming file connection. The uploader sends the transfer token first;
/// we answer with how much we already have.
pub(crate) async fn on_file_connection(inner: Arc<Inner>, username: String, mut conn: Conn) {
    let token = match tokio::time::timeout(Duration::from_secs(60), conn.u32()).await {
        Ok(Ok(t)) => t,
        _ => return,
    };
    let Some((_, d)) = inner.downloads.by_token.remove(&token) else {
        tracing::debug!(%username, token, "file connection for an unknown transfer");
        return;
    };
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
            *d.error.lock() = None;
            inner
                .metrics
                .downloads_completed
                .fetch_add(1, Ordering::Relaxed);
            d.set(DownloadState::Completed);
        }
        Err(e) if d.state() == DownloadState::Cancelled => {
            tracing::debug!(error = %e, "download cancelled")
        }
        Err(e) => {
            let attempts = d.attempts.fetch_add(1, Ordering::Relaxed) + 1;
            *d.error.lock() = Some(e.to_string());
            if attempts >= MAX_ATTEMPTS {
                inner
                    .metrics
                    .downloads_failed
                    .fetch_add(1, Ordering::Relaxed);
                d.fail(e.to_string());
            } else {
                d.transition(DownloadState::Transferring, DownloadState::Queued);
            }
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
        file.set_len(0).await?;
        offset = 0;
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
        if d.state() == DownloadState::Cancelled {
            file.flush().await?;
            return Err(std::io::Error::other("cancelled").into());
        }
        let want = buf.len().min((size - offset) as usize);
        let n = tokio::time::timeout(Duration::from_secs(120), conn.stream.read(&mut buf[..want]))
            .await
            .map_err(|_| Error::TimedOut)??;
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
