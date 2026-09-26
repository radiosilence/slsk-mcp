//! A token bucket shared by every transfer in one direction.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

use parking_lot::Mutex;

pub struct Bucket {
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl Bucket {
    pub fn new(rate: u64) -> Self {
        Self {
            rate: AtomicU64::new(rate),
            state: Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// Wait until `n` bytes may pass. Free when unlimited.
    pub async fn take(&self, n: usize) {
        loop {
            let rate = self.rate.load(Ordering::Relaxed);
            if rate == 0 {
                return;
            }
            let wait = {
                let mut s = self.state.lock();
                let now = Instant::now();
                // A quarter-second of burst keeps many small transfers from
                // serialising behind each other's sleeps.
                let burst = rate as f64 / 4.0;
                s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * rate as f64)
                    .min(burst.max(n as f64));
                s.1 = now;
                if s.0 >= n as f64 {
                    s.0 -= n as f64;
                    return;
                }
                Duration::from_secs_f64((n as f64 - s.0) / rate as f64)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn holds_throughput_to_the_rate() {
        let bucket = Bucket::new(1_000_000);
        let start = tokio::time::Instant::now();
        for _ in 0..40 {
            bucket.take(100_000).await;
        }
        let secs = start.elapsed().as_secs_f64();
        assert!((3.5..4.5).contains(&secs), "{secs}");
    }
}
