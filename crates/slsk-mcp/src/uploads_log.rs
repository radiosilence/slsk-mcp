//! Records every finished upload, so the uploads page has a history that
//! outlives the engine's in-memory list of recent transfers and restarts.
//!
//! The engine announces no upload's end, so this watches its transfer list:
//! an upload is timed from the first moment it is seen sending, and written
//! once when it reaches completed, failed or cancelled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::App;

const POLL: Duration = Duration::from_secs(2);
const KEEP_DAYS: i32 = 180;

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut started: HashMap<u64, Instant> = HashMap::new();
        let mut recorded: HashSet<u64> = HashSet::new();
        let mut pruned = Instant::now() - Duration::from_secs(3600);
        loop {
            tokio::time::sleep(POLL).await;
            let Some(engine) = app.session.engine() else {
                continue;
            };
            let uploads = engine.uploads();
            let live: HashSet<u64> = uploads.iter().map(|t| t.id).collect();
            for t in &uploads {
                match t.state {
                    "transferring" => {
                        started.entry(t.id).or_insert_with(Instant::now);
                    }
                    "completed" | "failed" | "cancelled" if !recorded.contains(&t.id) => {
                        let seconds = started.get(&t.id).map(|s| s.elapsed().as_secs_f64());
                        let name = t.filename.to_string_lossy();
                        match crate::db::record_upload(
                            &app.db,
                            &t.username,
                            &name,
                            t.size,
                            t.bytes,
                            t.state,
                            t.error.as_deref(),
                            seconds,
                        )
                        .await
                        {
                            Ok(()) => {
                                recorded.insert(t.id);
                            }
                            Err(e) => tracing::warn!(error = %e, "could not record an upload"),
                        }
                    }
                    _ => {}
                }
            }
            // Only ids the engine still lists can come round again.
            started.retain(|id, _| live.contains(id));
            recorded.retain(|id| live.contains(id));
            if pruned.elapsed() > Duration::from_secs(3600) {
                pruned = Instant::now();
                if let Err(e) = crate::db::prune_uploads(&app.db, KEEP_DAYS).await {
                    tracing::warn!(error = %e, "could not prune upload history");
                }
            }
        }
    });
}
