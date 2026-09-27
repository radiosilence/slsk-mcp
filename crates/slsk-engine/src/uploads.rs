//! Uploads: a queue per user, served round-robin with privileged users first,
//! one active upload per user, and a fixed number of slots overall.
//!
//! Round-robin is the difference from a single FIFO: one peer queueing their
//! way through a whole discography otherwise holds every slot until they are
//! done, and everyone behind them waits hours for a single track.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use slsk_proto::peer::{PeerMessage, file_transfer_init};
use slsk_proto::server::ToServer;
use slsk_proto::{ConnKind, RawStr, reason};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, oneshot};

use crate::net::READ_CHUNK;
use crate::{Direction, Error, Inner, Key, TransferView, peers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UploadState {
    Queued = 0,
    Starting = 1,
    Transferring = 2,
    Completed = 3,
    Failed = 4,
    Cancelled = 5,
}

impl UploadState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Queued,
            1 => Self::Starting,
            2 => Self::Transferring,
            3 => Self::Completed,
            4 => Self::Failed,
            _ => Self::Cancelled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Transferring => "transferring",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

pub(crate) struct Upload {
    id: u64,
    username: String,
    filename: RawStr,
    path: PathBuf,
    size: u64,
    bytes: AtomicU64,
    speed: AtomicU64,
    state: AtomicU8,
    error: Mutex<Option<String>>,
}

impl Upload {
    fn state(&self) -> UploadState {
        UploadState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set(&self, s: UploadState) {
        self.state.store(s as u8, Ordering::Release);
    }

    fn view(&self, place: Option<u32>) -> TransferView {
        let state = self.state();
        TransferView {
            id: self.id,
            direction: Direction::Upload,
            username: self.username.clone(),
            filename: self.filename.clone(),
            size: self.size,
            bytes: self.bytes.load(Ordering::Relaxed),
            speed: if state == UploadState::Transferring {
                self.speed.load(Ordering::Relaxed)
            } else {
                0
            },
            state: state.as_str(),
            place,
            error: self.error.lock().clone(),
            path: None,
        }
    }
}

#[derive(Default)]
struct Queue {
    /// Users in the order they will next be served.
    order: VecDeque<String>,
    by_user: HashMap<String, VecDeque<Arc<Upload>>>,
    active: HashMap<Key, Arc<Upload>>,
    /// Finished uploads, newest last, for display.
    history: VecDeque<Arc<Upload>>,
}

pub(crate) struct Uploads {
    queue: Mutex<Queue>,
    slots: AtomicUsize,
    wake: Notify,
    responses: DashMap<u32, oneshot::Sender<(bool, Option<String>)>>,
    next_id: AtomicU64,
    /// Bytes per second of the last completed upload, which is what the
    /// protocol reports as our speed.
    last_speed: AtomicU64,
    served: Mutex<HashSet<String>>,
}

const HISTORY: usize = 500;

impl Uploads {
    pub fn new(slots: usize) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            slots: AtomicUsize::new(slots.max(1)),
            wake: Notify::new(),
            responses: DashMap::new(),
            next_id: AtomicU64::new(0),
            last_speed: AtomicU64::new(0),
            served: Mutex::new(HashSet::new()),
        }
    }

    pub fn set_slots(&self, slots: usize) {
        self.slots.store(slots.max(1), Ordering::Relaxed);
        self.wake.notify_one();
    }

    pub fn slots(&self) -> usize {
        self.slots.load(Ordering::Relaxed)
    }

    pub fn avg_speed(&self) -> u32 {
        self.last_speed
            .load(Ordering::Relaxed)
            .min(u64::from(u32::MAX)) as u32
    }

    /// (files waiting, whether a slot is free now)
    pub fn queue_summary(&self) -> (usize, bool) {
        let q = self.queue.lock();
        let waiting = q.by_user.values().map(VecDeque::len).sum();
        (
            waiting,
            waiting == 0 && q.active.len() < self.slots.load(Ordering::Relaxed),
        )
    }

    pub fn place(&self, username: &str, filename: &RawStr) -> Option<u32> {
        let q = self.queue.lock();
        let mine = q.by_user.get(username)?;
        let pos = mine.iter().position(|u| &u.filename == filename)?;
        // Round-robin: everyone ahead in the rotation gets one turn per round.
        let rounds_ahead = pos;
        let users_ahead = q
            .order
            .iter()
            .take_while(|u| u.as_str() != username)
            .count();
        Some((rounds_ahead * q.order.len() + users_ahead + 1) as u32)
    }

    pub fn on_response(&self, token: u32, allowed: bool, reason: Option<String>) {
        if let Some((_, tx)) = self.responses.remove(&token) {
            let _ = tx.send((allowed, reason));
        }
    }

    pub fn cancel(&self, username: &str, filename: &RawStr) -> bool {
        let mut q = self.queue.lock();
        if let Some(u) = q.active.get(&(username.to_string(), filename.0.clone())) {
            u.set(UploadState::Cancelled);
            return true;
        }
        if let Some(list) = q.by_user.get_mut(username)
            && let Some(i) = list.iter().position(|u| &u.filename == filename)
        {
            let u = list.remove(i).unwrap();
            u.set(UploadState::Cancelled);
            push_history(&mut q.history, u);
            return true;
        }
        false
    }

    pub fn drop_users(&self, users: &HashSet<String>) {
        let mut q = self.queue.lock();
        q.by_user.retain(|u, _| !users.contains(&u.to_lowercase()));
        q.order.retain(|u| !users.contains(&u.to_lowercase()));
        for u in q.active.values() {
            if users.contains(&u.username.to_lowercase()) {
                u.set(UploadState::Cancelled);
            }
        }
    }

    pub fn views(&self) -> Vec<TransferView> {
        let q = self.queue.lock();
        let mut out: Vec<TransferView> = q.active.values().map(|u| u.view(None)).collect();
        for list in q.by_user.values() {
            for (i, u) in list.iter().enumerate() {
                out.push(u.view(Some(i as u32 + 1)));
            }
        }
        out.extend(q.history.iter().map(|u| u.view(None)));
        out.sort_unstable_by_key(|t| t.id);
        out
    }
}

fn push_history(history: &mut VecDeque<Arc<Upload>>, u: Arc<Upload>) {
    if history.len() >= HISTORY {
        history.pop_front();
    }
    history.push_back(u);
}

/// A peer asked for a file. `Err` carries the rejection reason to send back.
pub(crate) fn enqueue(
    inner: &Arc<Inner>,
    username: &str,
    filename: RawStr,
) -> Result<(), &'static str> {
    if inner.is_banned(username) {
        return Err(reason::BANNED);
    }
    let (path, size) = {
        let index = inner.shares.load();
        let f = index.get(filename.as_bytes()).ok_or(reason::NOT_SHARED)?;
        (f.path.clone(), f.size)
    };
    let mut q = inner.uploads.queue.lock();
    if q.active
        .contains_key(&(username.to_string(), filename.0.clone()))
    {
        return Ok(());
    }
    let list = q.by_user.entry(username.to_string()).or_default();
    if list.iter().any(|u| u.filename == filename) {
        return Ok(());
    }
    if list.len() >= inner.cfg.max_queued_files_per_user {
        return Err(reason::TOO_MANY_FILES);
    }
    if list.iter().map(|u| u.size).sum::<u64>() + size > inner.cfg.max_queued_bytes_per_user {
        return Err(reason::TOO_MANY_MEGABYTES);
    }
    list.push_back(Arc::new(Upload {
        id: inner.uploads.next_id.fetch_add(1, Ordering::Relaxed),
        username: username.to_string(),
        filename,
        path,
        size,
        bytes: AtomicU64::new(0),
        speed: AtomicU64::new(0),
        state: AtomicU8::new(UploadState::Queued as u8),
        error: Mutex::new(None),
    }));
    if !q.order.iter().any(|u| u == username) {
        q.order.push_back(username.to_string());
    }
    let waiting: usize = q.by_user.values().map(VecDeque::len).sum();
    drop(q);
    inner
        .metrics
        .uploads_queued
        .store(waiting as u64, Ordering::Relaxed);
    inner.uploads.wake.notify_one();
    Ok(())
}

/// Start uploads whenever a slot is free.
pub(crate) async fn schedule(inner: Arc<Inner>) {
    loop {
        tokio::select! {
            _ = inner.uploads.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        loop {
            let next = {
                let mut q = inner.uploads.queue.lock();
                if q.active.len() >= inner.uploads.slots.load(Ordering::Relaxed) {
                    break;
                }
                pick(&mut q, &inner.privileged.read())
            };
            let Some(upload) = next else { break };
            tokio::spawn(run(inner.clone(), upload));
        }
    }
}

fn pick(q: &mut Queue, privileged: &HashSet<String>) -> Option<Arc<Upload>> {
    let busy: HashSet<String> = q.active.values().map(|u| u.username.clone()).collect();
    let eligible = |u: &String, q: &Queue| {
        !busy.contains(u) && q.by_user.get(u).is_some_and(|l| !l.is_empty())
    };
    let pos = q
        .order
        .iter()
        .position(|u| privileged.contains(&u.to_lowercase()) && eligible(u, q))
        .or_else(|| q.order.iter().position(|u| eligible(u, q)))?;
    let user = q.order.remove(pos).unwrap();
    let list = q.by_user.get_mut(&user).unwrap();
    let upload = list.pop_front().unwrap();
    if list.is_empty() {
        q.by_user.remove(&user);
    } else {
        q.order.push_back(user);
    }
    q.active.insert(
        (upload.username.clone(), upload.filename.0.clone()),
        upload.clone(),
    );
    Some(upload)
}

async fn run(inner: Arc<Inner>, u: Arc<Upload>) {
    u.set(UploadState::Starting);
    inner.metrics.uploads_active.fetch_add(1, Ordering::Relaxed);
    let result = send(&inner, &u).await;
    inner.metrics.uploads_active.fetch_sub(1, Ordering::Relaxed);
    match result {
        Ok(speed) => {
            u.set(UploadState::Completed);
            inner
                .metrics
                .uploads_completed
                .fetch_add(1, Ordering::Relaxed);
            inner.uploads.last_speed.store(speed, Ordering::Relaxed);
            let served = {
                let mut s = inner.uploads.served.lock();
                s.insert(u.username.to_lowercase());
                s.len()
            };
            inner
                .metrics
                .upload_users
                .store(served as u64, Ordering::Relaxed);
            let _ = inner.send_server(ToServer::SendUploadSpeed {
                speed: speed.min(u64::from(u32::MAX)) as u32,
            });
        }
        Err(e) => {
            if u.state() != UploadState::Cancelled {
                u.set(UploadState::Failed);
            }
            *u.error.lock() = Some(e.to_string());
            inner.metrics.uploads_failed.fetch_add(1, Ordering::Relaxed);
            if u.bytes.load(Ordering::Relaxed) > 0 {
                let frame = PeerMessage::UploadFailed {
                    filename: u.filename.clone(),
                }
                .encode();
                let inner = inner.clone();
                let user = u.username.clone();
                tokio::spawn(async move {
                    let _ = peers::send(&inner, &user, frame).await;
                });
            }
        }
    }
    let mut q = inner.uploads.queue.lock();
    if let Some(done) = q.active.remove(&(u.username.clone(), u.filename.0.clone())) {
        push_history(&mut q.history, done);
    }
    let waiting: usize = q.by_user.values().map(VecDeque::len).sum();
    drop(q);
    inner
        .metrics
        .uploads_queued
        .store(waiting as u64, Ordering::Relaxed);
    inner.uploads.wake.notify_one();
}

/// Offer the file, open the file connection, stream from the requested
/// offset. Returns the average speed in bytes per second.
async fn send(inner: &Arc<Inner>, u: &Upload) -> Result<u64, Error> {
    let token = inner.next_token();
    let (tx, rx) = oneshot::channel();
    inner.uploads.responses.insert(token, tx);
    let offer = PeerMessage::TransferRequest {
        direction: 1,
        token,
        filename: u.filename.clone(),
        size: Some(u.size),
    };
    let answer = async {
        peers::send(inner, &u.username, offer.encode()).await?;
        tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .map_err(|_| Error::TimedOut)?
            .map_err(|_| Error::TimedOut)
    }
    .await;
    inner.uploads.responses.remove(&token);
    let (allowed, reason) = answer?;
    if !allowed {
        return Err(std::io::Error::other(reason.unwrap_or_else(|| "refused".into())).into());
    }

    let mut file = tokio::fs::File::open(&u.path)
        .await
        .map_err(|_| std::io::Error::other(reason::READ_ERROR))?;
    let mut conn = peers::connect(inner, &u.username, ConnKind::File).await?;
    conn.stream.write_all(&file_transfer_init(token)).await?;
    let offset = tokio::time::timeout(Duration::from_secs(60), conn.u64())
        .await
        .map_err(|_| Error::TimedOut)??;
    if offset > u.size {
        return Err(std::io::Error::other("offset beyond end of file").into());
    }
    file.seek(SeekFrom::Start(offset)).await?;
    u.bytes.store(offset, Ordering::Relaxed);
    u.set(UploadState::Transferring);

    let started = Instant::now();
    let mut sent = offset;
    let mut window = (Instant::now(), offset);
    let mut buf = vec![0u8; READ_CHUNK];
    while sent < u.size {
        if u.state() == UploadState::Cancelled {
            return Err(std::io::Error::other(reason::CANCELLED).into());
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Err(std::io::Error::other(reason::READ_ERROR).into());
        }
        inner.up_limit.take(n).await;
        tokio::time::timeout(Duration::from_secs(120), conn.stream.write_all(&buf[..n]))
            .await
            .map_err(|_| Error::TimedOut)??;
        sent += n as u64;
        u.bytes.store(sent, Ordering::Relaxed);
        inner
            .metrics
            .bytes_uploaded
            .fetch_add(n as u64, Ordering::Relaxed);
        let elapsed = window.0.elapsed();
        if elapsed >= Duration::from_secs(1) {
            u.speed.store(
                ((sent - window.1) as f64 / elapsed.as_secs_f64()) as u64,
                Ordering::Relaxed,
            );
            window = (Instant::now(), sent);
        }
    }
    // The downloader closes the connection once it has everything. Wait for
    // that rather than closing first, which some clients read as a failure.
    let mut probe = [0u8; 1];
    let _ = tokio::time::timeout(Duration::from_secs(30), conn.stream.read(&mut probe)).await;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    Ok(((sent - offset) as f64 / secs) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(user: &str, name: &str, id: u64) -> Arc<Upload> {
        Arc::new(Upload {
            id,
            username: user.into(),
            filename: name.into(),
            path: PathBuf::new(),
            size: 1,
            bytes: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            state: AtomicU8::new(0),
            error: Mutex::new(None),
        })
    }

    fn queue(entries: &[(&str, &str)]) -> Queue {
        let mut q = Queue::default();
        for (i, (user, name)) in entries.iter().enumerate() {
            q.by_user
                .entry(user.to_string())
                .or_default()
                .push_back(upload(user, name, i as u64));
            if !q.order.iter().any(|u| u == user) {
                q.order.push_back(user.to_string());
            }
        }
        q
    }

    #[test]
    fn serves_users_round_robin_one_active_each() {
        let mut q = queue(&[("a", "1"), ("a", "2"), ("a", "3"), ("b", "1"), ("c", "1")]);
        let none = HashSet::new();
        let served: Vec<String> = (0..3)
            .map(|_| pick(&mut q, &none).unwrap().username.clone())
            .collect();
        assert_eq!(served, ["a", "b", "c"]);
        assert!(
            pick(&mut q, &none).is_none(),
            "a already has an active upload"
        );
        q.active.clear();
        assert_eq!(pick(&mut q, &none).unwrap().filename.to_string_lossy(), "2");
    }

    #[test]
    fn privileged_users_go_first() {
        let mut q = queue(&[("a", "1"), ("b", "1"), ("vip", "1")]);
        let vip = HashSet::from(["vip".to_string()]);
        assert_eq!(pick(&mut q, &vip).unwrap().username, "vip");
    }
}
