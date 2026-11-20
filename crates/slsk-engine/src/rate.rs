//! The total upload and download speeds, smoothed. Soulseek transfers stop
//! and start constantly (a file ends, the next is negotiated, a peer stalls),
//! so the sum of what is transferring this second jumps between nothing and
//! everything. The totals are instead measured from the byte counters once a
//! second and averaged exponentially, so a gap decays rather than drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::Metrics;

const TICK: Duration = Duration::from_secs(1);
/// The averaging time constant: a burst or a gap moves the reading about
/// two thirds of the way in this long.
const TAU: f64 = 5.0;
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
    let next = rate + (1.0 - (-secs / TAU).exp()) * (bytes as f64 / secs - rate);
    if bytes == 0 && next < FLOOR {
        0.0
    } else {
        next
    }
}

pub(crate) async fn sample(metrics: Arc<Metrics>, rates: Arc<Rates>) {
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
        let shown = (up as u64, down as u64);
        if shown != rates.get() {
            rates.up.store(shown.0, Ordering::Relaxed);
            rates.down.store(shown.1, Ordering::Relaxed);
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
        assert!(after_gap > 750_000.0 && after_gap < 850_000.0);
    }

    #[test]
    fn idle_settles_at_nothing() {
        let r = (0..60).fold(1_000_000.0, |r, _| step(r, 0, 1.0));
        assert_eq!(r, 0.0);
    }

    #[test]
    fn a_slow_trickle_is_not_floored() {
        let r = (0..60).fold(0.0, |r, _| step(r, 500, 1.0));
        assert!(r > 400.0);
    }
}
