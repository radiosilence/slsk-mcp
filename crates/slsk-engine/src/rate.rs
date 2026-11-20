//! Transfer speeds, measured once a second from bytes moved rather than in
//! the transfer loops, so a transfer that stalls reads as stalled. Each
//! transfer's speed is its last second's; the totals are smoothed. Soulseek transfers stop
//! and start constantly (a file ends, the next is negotiated, a peer stalls),
//! so the sum of what is transferring this second jumps between nothing and
//! everything. The totals are instead measured from the byte counters once a
//! second and averaged exponentially, so a gap decays rather than drops. A
//! rising rate is followed quickly, so a short transfer still reads close to
//! its real speed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::Inner;

const TICK: Duration = Duration::from_secs(1);
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

/// Store each live transfer's speed over the last `secs`, given its byte
/// count then (`seen`, by id, replaced with the counts now). A transfer seen
/// for the first time keeps its speed until the next call. True when any
/// transfer moved or changed speed, which a person watching would see.
pub(crate) fn measure<'a>(
    seen: &mut HashMap<u64, u64>,
    live: impl Iterator<Item = (u64, &'a AtomicU64, &'a AtomicU64)>,
    secs: f64,
) -> bool {
    let mut now = HashMap::new();
    let mut changed = false;
    for (id, bytes, speed) in live {
        let b = bytes.load(Ordering::Relaxed);
        if let Some(&was) = seen.get(&id) {
            let s = (b.saturating_sub(was) as f64 / secs) as u64;
            changed |= speed.swap(s, Ordering::Relaxed) != s || b != was;
        }
        now.insert(id, b);
    }
    *seen = now;
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
    let (mut uploads, mut downloads) = (HashMap::new(), HashMap::new());
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
        let mut changed = inner.uploads.measure(&mut uploads, secs);
        changed |= inner.downloads.measure(&mut downloads, secs);
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

    #[test]
    fn a_stalled_transfer_reads_nothing() {
        let (bytes, speed) = (AtomicU64::new(0), AtomicU64::new(0));
        let mut seen = HashMap::new();
        let mut tick = |b: u64| {
            bytes.store(b, Ordering::Relaxed);
            measure(&mut seen, [(1, &bytes, &speed)].into_iter(), 1.0);
            speed.load(Ordering::Relaxed)
        };
        assert_eq!(tick(0), 0);
        assert_eq!(tick(500_000), 500_000);
        assert_eq!(tick(500_000), 0);
    }

    #[test]
    fn a_slow_trickle_is_not_floored() {
        let r = (0..60).fold(0.0, |r, _| step(r, 500, 1.0));
        assert!(r > 400.0);
    }
}
