//! Transfer speeds, measured once a second from the bytes each transfer and
//! the whole client have moved.
//!
//! Each transfer's speed is what it moved over the last few seconds, so a
//! transfer that stalls reads as stalled and a slow one, whose bytes arrive
//! in bursts, still reads steadily. The totals are averaged exponentially:
//! Soulseek transfers stop and start constantly (a file ends, the next is
//! negotiated, a peer stalls), and an exponential average lets a gap decay
//! rather than drop to nothing. A rising total is followed quickly, so a
//! short transfer still reads close to its real speed.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::Inner;

const TICK: Duration = Duration::from_secs(1);
/// How far back a transfer's own speed looks.
const WINDOW: Duration = Duration::from_secs(3);
/// The averaging time constants, rising and falling: a change moves the
/// reading about two thirds of the way in this long.
const RISE: f64 = 1.0;
const FALL: f64 = 2.0;
/// Below this, with nothing moving, the reading is shown as nothing rather
/// than decaying through ever smaller numbers.
const FLOOR: f64 = 1024.0;

#[derive(Default)]
pub(crate) struct Rates {
    up: AtomicU64,
    down: AtomicU64,
}

impl Rates {
    pub(crate) fn get(&self) -> (u64, u64) {
        (
            self.up.load(Ordering::Relaxed),
            self.down.load(Ordering::Relaxed),
        )
    }
}

/// One step of the average: `rate` after `bytes` moved in `secs`.
fn step(rate: f64, bytes: u64, secs: f64) -> f64 {
    let now = bytes as f64 / secs;
    let tau = if now > rate { RISE } else { FALL };
    let next = rate + (1.0 - (-secs / tau).exp()) * (now - rate);
    if bytes == 0 && next < FLOOR {
        0.0
    } else {
        next
    }
}

/// Each live transfer's byte counts over the last [`WINDOW`], by id.
pub(crate) type Seen = HashMap<u64, VecDeque<(Instant, u64)>>;

/// Store each live transfer's speed over the last [`WINDOW`], from its byte
/// counts at earlier calls (`seen`, which keeps only live transfers). A
/// transfer seen for the first time reads 0 until there is an interval to
/// measure. True when any transfer moved or changed speed, which a person
/// watching would see.
pub(crate) fn measure<'a>(
    seen: &mut Seen,
    live: impl Iterator<Item = (u64, &'a AtomicU64, &'a AtomicU64)>,
    now: Instant,
) -> bool {
    let mut kept = Seen::new();
    let mut changed = false;
    for (id, bytes, speed) in live {
        let b = bytes.load(Ordering::Relaxed);
        let mut counts = seen.remove(&id).unwrap_or_default();
        let moved = counts.back().is_some_and(|&(_, last)| last != b);
        counts.push_back((now, b));
        while counts.len() > 2 && now.duration_since(counts[1].0) >= WINDOW {
            counts.pop_front();
        }
        let (at, first) = counts[0];
        let secs = now.duration_since(at).as_secs_f64();
        let s = if secs > 0.0 {
            (b.saturating_sub(first) as f64 / secs) as u64
        } else {
            0
        };
        changed |= speed.swap(s, Ordering::Relaxed) != s || moved;
        kept.insert(id, counts);
    }
    *seen = kept;
    changed
}

pub(crate) async fn sample(inner: Arc<Inner>) {
    let (metrics, rates) = (&inner.metrics, &inner.rates);
    let read = || {
        (
            metrics.bytes_uploaded.load(Ordering::Relaxed),
            metrics.bytes_downloaded.load(Ordering::Relaxed),
        )
    };
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = (tokio::time::Instant::now(), read());
    let (mut up, mut down) = (0.0, 0.0);
    let (mut uploads, mut downloads) = (Seen::new(), Seen::new());
    loop {
        tick.tick().await;
        let now = tokio::time::Instant::now();
        let bytes = read();
        let secs = now.duration_since(last.0).as_secs_f64();
        if secs <= 0.0 {
            continue;
        }
        up = step(up, bytes.0 - last.1.0, secs);
        down = step(down, bytes.1 - last.1.1, secs);
        last = (now, bytes);
        let mut changed = inner.uploads.measure(&mut uploads, now);
        changed |= inner.downloads.measure(&mut downloads, now);
        let shown = (up as u64, down as u64);
        if shown != rates.get() {
            rates.up.store(shown.0, Ordering::Relaxed);
            rates.down.store(shown.1, Ordering::Relaxed);
            changed = true;
        }
        if changed {
            crate::changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_decays_rather_than_drops() {
        let steady = (0..60).fold(0.0, |r, _| step(r, 1_000_000, 1.0));
        assert!((steady - 1_000_000.0).abs() < 1_000.0);
        let after_gap = step(steady, 0, 1.0);
        assert!(after_gap > 550_000.0 && after_gap < 650_000.0);
    }

    #[test]
    fn a_start_is_followed_quickly() {
        let r = (0..3).fold(0.0, |r, _| step(r, 1_000_000, 1.0));
        assert!(r > 900_000.0);
    }

    #[test]
    fn idle_settles_at_nothing() {
        let r = (0..60).fold(1_000_000.0, |r, _| step(r, 0, 1.0));
        assert_eq!(r, 0.0);
    }

    /// Each transfer's speed after it reaches each of `counts`, a second
    /// apart.
    fn speeds(counts: &[u64]) -> Vec<u64> {
        let (bytes, speed) = (AtomicU64::new(0), AtomicU64::new(7));
        let mut seen = Seen::new();
        let start = Instant::now();
        counts
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                bytes.store(b, Ordering::Relaxed);
                let at = start + Duration::from_secs(i as u64);
                measure(&mut seen, [(1, &bytes, &speed)].into_iter(), at);
                speed.load(Ordering::Relaxed)
            })
            .collect()
    }

    #[test]
    fn a_new_transfer_reads_nothing_until_measured() {
        assert_eq!(speeds(&[100]), [0]);
    }

    #[test]
    fn a_stalled_transfer_reads_nothing_within_the_window() {
        let s = speeds(&[0, 300_000, 600_000, 900_000, 900_000, 900_000, 900_000]);
        assert_eq!(s[3], 300_000);
        assert_eq!(*s.last().unwrap(), 0);
    }

    #[test]
    fn a_transfer_moving_in_bursts_reads_steadily() {
        let chunk = 262_144;
        let s = speeds(&[0, chunk, chunk, 2 * chunk, 2 * chunk, 3 * chunk, 3 * chunk]);
        assert!(s[3..].iter().all(|&v| v > 60_000 && v < 180_000), "{s:?}");
    }

    #[test]
    fn a_slow_trickle_is_not_floored() {
        let r = (0..60).fold(0.0, |r, _| step(r, 500, 1.0));
        assert!(r > 400.0);
    }
}
