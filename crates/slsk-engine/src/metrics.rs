//! Counters and gauges, rendered in the Prometheus text format by the host.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub bytes_uploaded: AtomicU64,
    pub bytes_downloaded: AtomicU64,
    pub uploads_completed: AtomicU64,
    pub uploads_failed: AtomicU64,
    pub downloads_completed: AtomicU64,
    pub downloads_failed: AtomicU64,
    pub searches_sent: AtomicU64,
    pub search_requests: AtomicU64,
    pub search_responses_sent: AtomicU64,
    pub search_responses_dropped: AtomicU64,
    pub distributed_forwarded: AtomicU64,
    pub peer_connections: AtomicU64,
    pub shared_files: AtomicU64,
    pub children: AtomicU64,
    pub uploads_active: AtomicU64,
    pub uploads_queued: AtomicU64,
    pub downloads_active: AtomicU64,
}

impl Metrics {
    pub fn render(&self, out: &mut String) {
        let counters = [
            (
                "slsk_uploaded_bytes_total",
                "Bytes sent to peers.",
                &self.bytes_uploaded,
            ),
            (
                "slsk_downloaded_bytes_total",
                "Bytes received from peers.",
                &self.bytes_downloaded,
            ),
            (
                "slsk_uploads_completed_total",
                "Uploads finished.",
                &self.uploads_completed,
            ),
            (
                "slsk_uploads_failed_total",
                "Uploads that failed or were refused.",
                &self.uploads_failed,
            ),
            (
                "slsk_downloads_completed_total",
                "Downloads finished.",
                &self.downloads_completed,
            ),
            (
                "slsk_downloads_failed_total",
                "Downloads that failed.",
                &self.downloads_failed,
            ),
            (
                "slsk_searches_sent_total",
                "Searches we started.",
                &self.searches_sent,
            ),
            (
                "slsk_search_requests_total",
                "Searches from other users that reached us.",
                &self.search_requests,
            ),
            (
                "slsk_search_responses_total",
                "Searches we answered with results.",
                &self.search_responses_sent,
            ),
            (
                "slsk_search_responses_dropped_total",
                "Matching searches dropped under load.",
                &self.search_responses_dropped,
            ),
            (
                "slsk_distributed_forwarded_total",
                "Searches forwarded to distributed children.",
                &self.distributed_forwarded,
            ),
        ];
        for (name, help, v) in counters {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
                v.load(Ordering::Relaxed)
            );
        }
        let gauges = [
            (
                "slsk_peer_connections",
                "Open peer message connections.",
                &self.peer_connections,
            ),
            (
                "slsk_shared_files",
                "Files in the share index.",
                &self.shared_files,
            ),
            (
                "slsk_distributed_children",
                "Distributed-network children.",
                &self.children,
            ),
            (
                "slsk_uploads_active",
                "Uploads in progress.",
                &self.uploads_active,
            ),
            (
                "slsk_uploads_queued",
                "Uploads waiting for a slot.",
                &self.uploads_queued,
            ),
            (
                "slsk_downloads_active",
                "Downloads in progress.",
                &self.downloads_active,
            ),
        ];
        for (name, help, v) in gauges {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {}",
                v.load(Ordering::Relaxed)
            );
        }
    }
}
