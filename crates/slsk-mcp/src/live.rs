//! What changed, for the pages that show it as it happens.
//!
//! Each kind of thing a page shows has a counter, bumped whenever something
//! of that kind changes: a database write to its tables, or a stage an
//! import reaches. A page's stream waits on these instead of polling, and
//! redraws only the parts whose counters moved.
//!
//! A write is announced once the database call that made it has finished,
//! so its transaction has committed and a stream woken by it reads it.

use std::sync::LazyLock;

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    /// Jobs, their files and their history, and what imports are doing.
    Jobs = 0,
    /// Uploads recorded as sent.
    Uploads = 1,
    /// Messages, buddies, rooms and the wishlist.
    Social = 2,
    /// Search tabs.
    Search = 3,
    /// A run over the library (re-file, re-match, credits).
    Library = 4,
    /// Other users browsing, searching or downloading from our shares.
    Activity = 5,
}

/// Transfers, the search budget and the distributed network are not topics
/// here: the engine has its own signal, [`slsk_engine::changes`].
pub const TOPICS: usize = 6;

pub type Counts = [u64; TOPICS];

static CHANGES: LazyLock<watch::Sender<Counts>> = LazyLock::new(|| watch::channel([0; TOPICS]).0);

/// Something of `topic` changed.
pub fn changed(topic: Topic) {
    CHANGES.send_modify(|c| c[topic as usize] += 1);
}

pub fn subscribe() -> watch::Receiver<Counts> {
    CHANGES.subscribe()
}

/// State held in memory that a page shows: a writer's lock announces the
/// change when it is let go, a reader's does not.
#[derive(Default)]
pub struct Watched<T> {
    topic: Option<Topic>,
    inner: parking_lot::Mutex<T>,
}

impl<T> Watched<T> {
    pub fn new(topic: Topic, value: T) -> Self {
        Self {
            topic: Some(topic),
            inner: parking_lot::Mutex::new(value),
        }
    }

    pub fn lock(&self) -> Writing<'_, T> {
        Writing {
            topic: self.topic,
            guard: self.inner.lock(),
        }
    }

    pub fn read(&self) -> parking_lot::MutexGuard<'_, T> {
        self.inner.lock()
    }
}

pub struct Writing<'a, T> {
    topic: Option<Topic>,
    guard: parking_lot::MutexGuard<'a, T>,
}

impl<T> std::ops::Deref for Writing<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for Writing<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for Writing<'_, T> {
    fn drop(&mut self) {
        if let Some(topic) = self.topic {
            changed(topic);
        }
    }
}

/// The topic a write to `table` changes, if a page shows it. SQLite reports
/// no writes to `WITHOUT ROWID` tables, so `job_files` is noted by the
/// functions that write it ([`note`]).
pub fn of_table(table: &str) -> Option<Topic> {
    Some(match table {
        "jobs" | "job_events" => Topic::Jobs,
        "uploads" => Topic::Uploads,
        "messages" | "wishes" => Topic::Social,
        "search_tabs" => Topic::Search,
        "share_events" => Topic::Activity,
        _ => return None,
    })
}

thread_local! {
    /// Topics written by the database call running on this thread, not yet
    /// announced.
    static PENDING: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Something of `topic` is being written by the database call on this
/// thread; announced by [`announce`] when the call has finished.
pub fn note(topic: Topic) {
    PENDING.with(|p| p.set(p.get() | 1 << topic as u8));
}

/// Announce what the database call on this thread wrote.
pub fn announce() {
    let pending = PENDING.with(|p| p.replace(0));
    if pending == 0 {
        return;
    }
    CHANGES.send_modify(|c| {
        for (i, n) in c.iter_mut().enumerate() {
            if pending & 1 << i != 0 {
                *n += 1;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_noted_write_is_announced_only_when_the_call_ends() {
        let rx = subscribe();
        let before = *rx.borrow();
        note(Topic::Uploads);
        assert_eq!(
            rx.borrow()[Topic::Uploads as usize],
            before[Topic::Uploads as usize]
        );
        announce();
        assert!(rx.borrow()[Topic::Uploads as usize] > before[Topic::Uploads as usize]);
    }

    #[tokio::test]
    async fn a_change_wakes_a_subscriber_with_its_topic_counted() {
        let mut rx = subscribe();
        let before = *rx.borrow_and_update();
        changed(Topic::Search);
        rx.changed().await.unwrap();
        let after = *rx.borrow_and_update();
        assert!(after[Topic::Search as usize] > before[Topic::Search as usize]);
    }
}
