//! SQLite: accounts, jobs, bans, messages and the rest of what must survive a
//! restart. Transfer progress is not here: it lives in the engine's memory and
//! is read from there.
//!
//! One file in the state directory, opened once. Reads go through a pool of
//! read-only connections, which WAL lets run beside a write; every write goes
//! through a single connection, so writes queue in the process instead of
//! contending for SQLite's lock. sqlx runs each connection on its own thread,
//! so a query never occupies an async worker.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::types::Json;
use uuid::Uuid;

/// Completed uploads since a time.
pub(crate) const UPLOAD_TOTALS: &str = "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM uploads \
         WHERE state = 'completed' AND finished_at >= ?1";

/// The latest finished uploads.
pub(crate) const RECENT_UPLOADS: &str = "SELECT username, filename, size, bytes, state, seconds, finished_at FROM uploads \
         ORDER BY finished_at DESC LIMIT ?1";

/// Peers that stalled a download since a time.
pub(crate) const STALLED_PEERS: &str = "SELECT DISTINCT coalesce(e.peer, j.source->>'username') FROM job_events e \
         LEFT JOIN jobs j ON j.id = e.job_id \
         WHERE e.cause = 'stalled_peer' AND e.at > ?1 \
         AND coalesce(e.peer, j.source->>'username') IS NOT NULL";

/// Events with a cause, newest first.
pub(crate) const EVENTS_SINCE: &str = "SELECT job_id, title, at, version, outcome, cause, detail FROM job_events \
         WHERE cause IS NOT NULL AND at >= ?1 ORDER BY at DESC LIMIT 5000";

/// One file's state, before it changes.
pub(crate) const FILE_STATE: &str =
    "SELECT state, size FROM job_files WHERE job_id = ?1 AND peer = ?2 AND remote = ?3";

/// The files of several jobs, named by `id_list`.
pub(crate) const FILES_OF_JOBS: &str = "SELECT * FROM job_files WHERE job_id IN (SELECT unhex(value) FROM json_each(?1)) \
         ORDER BY job_id, subdir, remote";

/// One job's files.
pub(crate) const JOB_FILES: &str =
    "SELECT * FROM job_files WHERE job_id = ?1 ORDER BY subdir, remote";

/// A grab for the same query in the last day, unless it failed.
pub(crate) const LIVE_GRAB: &str = "SELECT * FROM jobs WHERE lower(title) = lower(?1) AND status <> 'failed' \
         AND created_at > ?2 ORDER BY created_at DESC LIMIT 1";

/// Newest first. Two statements rather than `?1 IS NULL OR status = ?1`,
/// which no index can answer.
pub(crate) const JOBS: &str = "SELECT * FROM jobs ORDER BY created_at DESC LIMIT ?1";
pub(crate) const JOBS_OF_STATUS: &str =
    "SELECT * FROM jobs WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2";

/// The current time as the schema stores it, for use inside SQL.
macro_rules! now {
    () => {
        "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')"
    };
}
pub(crate) use now;

#[derive(Clone)]
pub struct Db {
    pub read: SqlitePool,
    pub write: SqlitePool,
}

impl Db {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let base = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .pragma("temp_store", "memory");
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(base.clone().create_if_missing(true))
            .await?;
        sqlx::migrate!("./migrations").run(&write).await?;
        let read = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(base.read_only(true))
            .await?;
        let db = Self { read, write };
        db.optimize().await;
        Ok(db)
    }

    /// An in-memory database for tests: one shared connection, so the pools
    /// see the same data.
    #[cfg(test)]
    pub async fn memory() -> Self {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str("sqlite::memory:")
                    .unwrap()
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        Self {
            read: pool.clone(),
            write: pool,
        }
    }

    /// Refresh the planner's statistics where they have drifted. Bounded, so
    /// it costs milliseconds; without statistics SQLite can pick an index that
    /// only supplies the ordering and read the whole table.
    pub async fn optimize(&self) {
        if let Err(e) = sqlx::query("PRAGMA analysis_limit = 400; PRAGMA optimize = 0x10002;")
            .execute(&self.write)
            .await
        {
            tracing::warn!(error = %e, "database optimize failed");
        }
    }
}

/// `ids` as a parameter for `IN (SELECT unhex(value) FROM json_each(?))`:
/// one statement for any number of ids, so it is prepared once.
pub fn id_list(ids: &[Uuid]) -> String {
    serde_json::to_string(
        &ids.iter()
            .map(|i| i.simple().to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into())
}

pub async fn active_account(db: &Db) -> sqlx::Result<Option<(String, String)>> {
    sqlx::query_as("SELECT username, sealed_password FROM accounts WHERE active")
        .fetch_optional(&db.read)
        .await
}

pub async fn save_account(db: &Db, username: &str, sealed: &str) -> sqlx::Result<()> {
    let mut tx = db.write.begin().await?;
    sqlx::query("UPDATE accounts SET active = FALSE WHERE active AND username <> ?1")
        .bind(username)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        concat!("INSERT INTO accounts (username, sealed_password, active) VALUES (?1, ?2, TRUE)
         ON CONFLICT (username) DO UPDATE SET sealed_password = EXCLUDED.sealed_password, active = TRUE, updated_at = ", now!(), ""),
    )
    .bind(username)
    .bind(sealed)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A folder on a peer. `folder` is the peer's path, lossily decoded for
    /// display; `folder_raw` is the bytes to request it with.
    Soulseek {
        username: String,
        folder: String,
        folder_raw: Vec<u8>,
    },
    /// Files picked individually.
    Files { username: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternate {
    pub username: String,
    pub folder: String,
    pub folder_raw: Vec<u8>,
    /// What the search found in it, `(remote path, size)`: used when the peer
    /// will not list the folder, as for the first choice. Empty in rows
    /// written before it was kept.
    #[serde(default)]
    pub files: Vec<(Vec<u8>, u64)>,
}

impl From<crate::folders::Folder> for Alternate {
    fn from(f: crate::folders::Folder) -> Self {
        Self {
            files: f
                .files
                .iter()
                .map(|x| (x.remote.as_bytes().to_vec(), x.size))
                .collect(),
            username: f.username,
            folder: f.path,
            folder_raw: f.remote_path.as_bytes().to_vec(),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: Uuid,
    pub account: String,
    pub title: String,
    pub source: Json<Source>,
    pub alternates: Json<Vec<Alternate>>,
    pub status: String,
    pub error: Option<String>,
    pub import_log: Option<String>,
    pub candidates: Option<Json<Vec<sift::Candidate>>>,
    pub library_path: Option<String>,
    pub analysis: Option<Json<Vec<crate::analysis::TrackAnalysis>>>,
    pub approved: bool,
    pub as_is_blocker: Option<String>,
    /// Fetched to replace a copy already filed; see `grab(refetch)`.
    pub replaces: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobFile {
    pub job_id: Uuid,
    pub peer: String,
    pub remote: Vec<u8>,
    pub size: i64,
    pub subdir: String,
    pub state: String,
    pub error: Option<String>,
}

pub async fn insert_job(db: &Db, job: &Job, files: &[JobFile]) -> sqlx::Result<()> {
    let mut tx = db.write.begin().await?;
    sqlx::query("INSERT INTO jobs (id, account, title, source, alternates, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
        .bind(job.id)
        .bind(&job.account)
        .bind(&job.title)
        .bind(&job.source)
        .bind(&job.alternates)
        .bind(&job.status)
        .execute(&mut *tx)
        .await?;
    for f in files {
        sqlx::query("INSERT INTO job_files (job_id, peer, remote, size, subdir) VALUES (?1, ?2, ?3, ?4, ?5)")
            .bind(f.job_id)
            .bind(&f.peer)
            .bind(&f.remote)
            .bind(f.size)
            .bind(&f.subdir)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

pub async fn set_replaces(db: &Db, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET replaces = true WHERE id = ?1")
        .bind(id)
        .execute(&db.write)
        .await?;
    Ok(())
}

pub async fn replace_files(
    db: &Db,
    job: Uuid,
    source: &Source,
    alternates: &[Alternate],
    files: &[JobFile],
) -> sqlx::Result<()> {
    let mut tx = db.write.begin().await?;
    sqlx::query("DELETE FROM job_files WHERE job_id = ?1")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query(concat!("UPDATE jobs SET source = ?2, alternates = ?3, status = 'downloading', error = NULL, updated_at = ", now!(), " WHERE id = ?1"))
        .bind(job)
        .bind(Json(source))
        .bind(Json(alternates))
        .execute(&mut *tx)
        .await?;
    for f in files {
        sqlx::query("INSERT INTO job_files (job_id, peer, remote, size, subdir) VALUES (?1, ?2, ?3, ?4, ?5)")
            .bind(f.job_id)
            .bind(&f.peer)
            .bind(&f.remote)
            .bind(f.size)
            .bind(&f.subdir)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

pub async fn jobs(db: &Db, status: Option<&str>, limit: i64) -> sqlx::Result<Vec<Job>> {
    match status {
        Some(status) => sqlx::query_as(JOBS_OF_STATUS).bind(status).bind(limit),
        None => sqlx::query_as(JOBS).bind(limit),
    }
    .fetch_all(&db.read)
    .await
}

pub async fn job(db: &Db, id: Uuid) -> sqlx::Result<Option<Job>> {
    sqlx::query_as("SELECT * FROM jobs WHERE id = ?1")
        .bind(id)
        .fetch_optional(&db.read)
        .await
}

/// The newest job a grab for `query` made in the last day, unless it failed:
/// asking again for what is already on its way is the same request.
pub async fn live_grab(db: &Db, query: &str) -> sqlx::Result<Option<Job>> {
    sqlx::query_as(LIVE_GRAB)
        .bind(query.trim())
        .bind(chrono::Utc::now() - chrono::Duration::days(1))
        .fetch_optional(&db.read)
        .await
}

pub async fn job_files(db: &Db, id: Uuid) -> sqlx::Result<Vec<JobFile>> {
    sqlx::query_as(JOB_FILES).bind(id).fetch_all(&db.read).await
}

/// The files of every job in `ids`, by job, in one query.
pub async fn files_of_jobs(
    db: &Db,
    ids: &[Uuid],
) -> sqlx::Result<std::collections::HashMap<Uuid, Vec<JobFile>>> {
    let rows: Vec<JobFile> = sqlx::query_as(FILES_OF_JOBS)
        .bind(id_list(ids))
        .fetch_all(&db.read)
        .await?;
    let mut by_job: std::collections::HashMap<Uuid, Vec<JobFile>> = Default::default();
    for f in rows {
        by_job.entry(f.job_id).or_default().push(f);
    }
    Ok(by_job)
}

pub async fn set_status(db: &Db, id: Uuid, status: &str, error: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "UPDATE jobs SET status = ?2, error = ?3, updated_at = ",
        now!(),
        " WHERE id = ?1"
    ))
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(&db.write)
    .await
    .map(|_| ())
}

pub async fn set_import_log(db: &Db, id: Uuid, log: &str) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "UPDATE jobs SET import_log = ?2, updated_at = ",
        now!(),
        " WHERE id = ?1"
    ))
    .bind(id)
    .bind(log)
    .execute(&db.write)
    .await
    .map(|_| ())
}

pub async fn set_file_state(
    db: &Db,
    id: Uuid,
    peer: &str,
    remote: &[u8],
    state: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    // A file's bytes count toward the lifetime total once, on the way into
    // completed. The writer is one connection, so nothing changes the row
    // between reading and updating it.
    let mut tx = db.write.begin().await?;
    let prev: Option<(String, i64)> = sqlx::query_as(FILE_STATE)
        .bind(id)
        .bind(peer)
        .bind(remote)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE job_files SET state = ?4, error = ?5 WHERE job_id = ?1 AND peer = ?2 AND remote = ?3",
    )
    .bind(id)
    .bind(peer)
    .bind(remote)
    .bind(state)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    if let Some((was, size)) = prev
        && state == "completed"
        && was != "completed"
    {
        add_total(&mut tx, "downloaded_bytes", size).await?;
    }
    tx.commit().await
}

async fn add_total(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    name: &str,
    by: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO totals (name, value) VALUES (?1, ?2) \
         ON CONFLICT (name) DO UPDATE SET value = value + excluded.value",
    )
    .bind(name)
    .bind(by)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

pub async fn delete_job(db: &Db, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE id = ?1")
        .bind(id)
        .execute(&db.write)
        .await
        .map(|_| ())
}

pub async fn bans(db: &Db, account: &str) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT username FROM bans WHERE account = ?1 ORDER BY username")
        .bind(account)
        .fetch_all(&db.read)
        .await
}

pub async fn set_ban(db: &Db, account: &str, username: &str, banned: bool) -> sqlx::Result<()> {
    let q = if banned {
        "INSERT INTO bans (account, username) VALUES (?1, ?2) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM bans WHERE account = ?1 AND username = ?2"
    };
    sqlx::query(q)
        .bind(account)
        .bind(username)
        .execute(&db.write)
        .await
        .map(|_| ())
}

/// Append an outcome to the job's history. See `0004_events.sql`.
pub async fn record_event(
    db: &Db,
    job_id: Uuid,
    title: &str,
    peer: Option<&str>,
    outcome: &str,
    cause: Option<&str>,
    detail: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO job_events (job_id, title, version, outcome, cause, detail, peer) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(job_id)
    .bind(title)
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(outcome)
    .bind(cause)
    .bind(detail)
    .bind(peer)
    .execute(&db.write)
    .await
    .map(|_| ())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobEvent {
    pub job_id: Uuid,
    pub title: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub version: String,
    pub outcome: String,
    pub cause: Option<String>,
    pub detail: Option<String>,
}

/// Events with a cause since `since`, newest first.
pub async fn events_since(
    db: &Db,
    since: chrono::DateTime<chrono::Utc>,
) -> sqlx::Result<Vec<JobEvent>> {
    sqlx::query_as(EVENTS_SINCE)
        .bind(since)
        .fetch_all(&db.read)
        .await
}

/// Every event ever recorded, by outcome and cause: monotonic, so it serves
/// as a counter.
pub async fn event_counts(db: &Db) -> sqlx::Result<Vec<(String, Option<String>, i64)>> {
    sqlx::query_as("SELECT outcome, cause, count(*) FROM job_events GROUP BY 1, 2")
        .fetch_all(&db.read)
        .await
}

/// Peers that stalled a download in the last day. Events from before the
/// peer was recorded name it through their job's source.
pub async fn stalled_peers(db: &Db) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(STALLED_PEERS)
        .bind(chrono::Utc::now() - chrono::Duration::days(1))
        .fetch_all(&db.read)
        .await
}

pub async fn set_imported(db: &Db, id: Uuid, path: &str) -> sqlx::Result<()> {
    sqlx::query(concat!("UPDATE jobs SET status = 'imported', library_path = ?2, error = NULL, candidates = NULL, updated_at = ", now!(), " WHERE id = ?1"))
        .bind(id)
        .bind(path)
        .execute(&db.write)
        .await
        .map(|_| ())
}

pub async fn set_review(
    db: &Db,
    id: Uuid,
    reason: &str,
    candidates: &[sift::Candidate],
) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "UPDATE jobs SET status = 'review', error = ?2, candidates = ?3, updated_at = ",
        now!(),
        " WHERE id = ?1"
    ))
    .bind(id)
    .bind(reason)
    .bind(Json(candidates))
    .execute(&db.write)
    .await
    .map(|_| ())
}

pub async fn set_analysis(
    db: &Db,
    id: Uuid,
    analysis: &[crate::analysis::TrackAnalysis],
) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "UPDATE jobs SET analysis = ?2, updated_at = ",
        now!(),
        " WHERE id = ?1"
    ))
    .bind(id)
    .bind(Json(analysis))
    .execute(&db.write)
    .await
    .map(|_| ())
}

/// Move a job to `importing` if it is waiting on a decision. One statement,
/// so two requests for the same job cannot both succeed.
pub async fn claim_import(db: &Db, id: Uuid) -> sqlx::Result<bool> {
    sqlx::query(concat!(
        "UPDATE jobs SET status = 'importing', error = NULL, updated_at = ",
        now!(),
        " \
         WHERE id = ?1 AND status IN ('review', 'suspect', 'failed') RETURNING id"
    ))
    .bind(id)
    .fetch_optional(&db.write)
    .await
    .map(|row| row.is_some())
}

pub async fn set_as_is_blocker(db: &Db, id: Uuid, blocker: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET as_is_blocker = ?2 WHERE id = ?1")
        .bind(id)
        .bind(blocker)
        .execute(&db.write)
        .await
        .map(|_| ())
}

pub async fn set_approved(db: &Db, id: Uuid) -> sqlx::Result<()> {
    sqlx::query(concat!(
        "UPDATE jobs SET approved = TRUE, updated_at = ",
        now!(),
        " WHERE id = ?1"
    ))
    .bind(id)
    .execute(&db.write)
    .await
    .map(|_| ())
}

#[derive(sqlx::FromRow, Clone)]
pub struct UploadRow {
    pub username: String,
    pub filename: String,
    pub size: i64,
    pub bytes: i64,
    pub state: String,
    pub seconds: Option<f64>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

/// A finished upload, with how long it was seen sending when known.
pub async fn record_upload(
    db: &Db,
    t: &slsk_engine::TransferView,
    seconds: Option<f64>,
) -> sqlx::Result<()> {
    let mut tx = db.write.begin().await?;
    sqlx::query(
        "INSERT INTO uploads (username, filename, size, bytes, state, error, seconds) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&t.username)
    .bind(t.filename.to_string_lossy())
    .bind(t.size as i64)
    .bind(t.bytes as i64)
    .bind(t.state)
    .bind(t.error.as_deref())
    .bind(seconds)
    .execute(&mut *tx)
    .await?;
    add_total(&mut tx, "uploaded_bytes", t.bytes as i64).await?;
    add_total(&mut tx, &format!("uploads_{}", t.state), 1).await?;
    if t.state == "completed" {
        sqlx::query(concat!(
            "INSERT INTO served_users (username, first_served, last_served, uploads, bytes) \
             VALUES (lower(?1), ",
            now!(),
            ", ",
            now!(),
            ", 1, ?2) \
             ON CONFLICT (username) DO UPDATE SET last_served = ",
            now!(),
            ", \
                 uploads = served_users.uploads + 1, bytes = served_users.bytes + EXCLUDED.bytes"
        ))
        .bind(&t.username)
        .bind(t.bytes as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

pub async fn recent_uploads(db: &Db, limit: i64) -> sqlx::Result<Vec<UploadRow>> {
    sqlx::query_as(RECENT_UPLOADS)
        .bind(limit)
        .fetch_all(&db.read)
        .await
}

/// Completed uploads since `since`: how many, and how many bytes.
pub async fn upload_totals(
    db: &Db,
    since: chrono::DateTime<chrono::Utc>,
) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(UPLOAD_TOTALS)
        .bind(since)
        .fetch_one(&db.read)
        .await
}

/// History older than `days` goes; it is for looking back weeks, not years.
pub async fn prune_uploads(db: &Db, days: i32) -> sqlx::Result<u64> {
    Ok(sqlx::query("DELETE FROM uploads WHERE finished_at < ?1")
        .bind(chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
        .execute(&db.write)
        .await?
        .rows_affected())
}

/// Lifetime totals by name: `uploaded_bytes`, `downloaded_bytes`, and
/// `uploads_<state>`.
pub async fn totals(db: &Db) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as("SELECT name, value FROM totals ORDER BY name")
        .fetch_all(&db.read)
        .await
}

/// Distinct users an upload has finished to: in the last day, the last week,
/// and ever.
pub async fn served_user_counts(db: &Db) -> sqlx::Result<(i64, i64, i64)> {
    sqlx::query_as(
        "SELECT \
             COUNT(*) FILTER (WHERE last_served >= ?1), \
             COUNT(*) FILTER (WHERE last_served >= ?2), \
             COUNT(*) \
         FROM served_users",
    )
    .bind(chrono::Utc::now() - chrono::Duration::days(1))
    .bind(chrono::Utc::now() - chrono::Duration::days(7))
    .fetch_one(&db.read)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every query on a hot path is answered from an index. A full `SCAN`
    /// does not fail, it only grows with the data, so it is caught here.
    #[tokio::test]
    async fn hot_queries_do_not_scan() {
        let db = Db::memory().await;
        let hot = [
            JOBS,
            JOBS_OF_STATUS,
            LIVE_GRAB,
            JOB_FILES,
            FILES_OF_JOBS,
            FILE_STATE,
            EVENTS_SINCE,
            STALLED_PEERS,
            RECENT_UPLOADS,
            UPLOAD_TOTALS,
            crate::social::CONVERSATIONS,
            crate::social::LATEST_PER_PEER,
            crate::social::THREAD,
            crate::social::UNREAD,
            crate::social::NEXT_WISH,
            crate::auth::session::SESSION,
        ];
        for sql in hot {
            use sqlx::Row;
            let plan: Vec<String> =
                sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                    .fetch_all(&db.read)
                    .await
                    .unwrap()
                    .iter()
                    .map(|r| r.get("detail"))
                    .collect();
            // `SCAN t` alone reads every row; `SCAN t USING INDEX` walks an
            // index in order and stops at the LIMIT.
            let scans: Vec<_> = plan
                .iter()
                .filter(|d| {
                    d.starts_with("SCAN ") && !d.contains(" USING ") && !d.contains("VIRTUAL TABLE")
                })
                .collect();
            assert!(scans.is_empty(), "{sql}\n{plan:#?}");
        }
    }

    fn job(title: &str, status: &str) -> Job {
        Job {
            id: Uuid::new_v4(),
            account: "me".into(),
            title: title.into(),
            source: Json(Source::Files {
                username: "peer".into(),
            }),
            alternates: Json(Vec::new()),
            status: status.into(),
            error: None,
            import_log: None,
            candidates: None,
            library_path: None,
            analysis: None,
            approved: false,
            as_is_blocker: None,
            replaces: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn file(job: &Job, name: &str, size: i64) -> JobFile {
        JobFile {
            job_id: job.id,
            peer: "peer".into(),
            remote: name.as_bytes().to_vec(),
            size,
            subdir: String::new(),
            state: "queued".into(),
            error: None,
        }
    }

    #[tokio::test]
    async fn files_of_several_jobs_come_back_by_job() {
        let db = Db::memory().await;
        let (a, b, c) = (
            job("a", "downloading"),
            job("b", "downloading"),
            job("c", "downloading"),
        );
        insert_job(&db, &a, &[file(&a, "1", 1), file(&a, "2", 2)])
            .await
            .unwrap();
        insert_job(&db, &b, &[file(&b, "3", 3)]).await.unwrap();
        insert_job(&db, &c, &[file(&c, "4", 4)]).await.unwrap();
        let by_job = files_of_jobs(&db, &[a.id, b.id]).await.unwrap();
        assert_eq!(by_job[&a.id].len(), 2);
        assert_eq!(by_job[&b.id].len(), 1);
        assert!(!by_job.contains_key(&c.id));
        assert!(files_of_jobs(&db, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_file_counts_toward_downloaded_bytes_once() {
        let db = Db::memory().await;
        let j = job("a", "downloading");
        insert_job(&db, &j, &[file(&j, "1", 100)]).await.unwrap();
        for state in ["transferring", "completed", "completed"] {
            set_file_state(&db, j.id, "peer", b"1", state, None)
                .await
                .unwrap();
        }
        assert_eq!(
            totals(&db).await.unwrap(),
            vec![("downloaded_bytes".to_string(), 100)]
        );
        assert_eq!(job_files(&db, j.id).await.unwrap()[0].state, "completed");
    }

    #[tokio::test]
    async fn times_set_by_sql_and_by_rust_compare_as_times() {
        let db = Db::memory().await;
        let j = job("Burial Untrue", "downloading");
        insert_job(&db, &j, &[]).await.unwrap();
        // Created by the column default; looked for with a cutoff bound
        // from Rust.
        let found = live_grab(&db, "burial untrue").await.unwrap().unwrap();
        assert_eq!(found.id, j.id);
        assert!((chrono::Utc::now() - found.created_at).num_seconds() < 5);
        sqlx::query("UPDATE jobs SET created_at = ?1")
            .bind(chrono::Utc::now() - chrono::Duration::hours(25))
            .execute(&db.write)
            .await
            .unwrap();
        assert!(live_grab(&db, "burial untrue").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn jobs_list_by_status_newest_first() {
        let db = Db::memory().await;
        let (a, b) = (job("a", "downloading"), job("b", "imported"));
        insert_job(&db, &a, &[]).await.unwrap();
        insert_job(&db, &b, &[]).await.unwrap();
        set_status(&db, a.id, "importing", None).await.unwrap();
        assert_eq!(jobs(&db, None, 10).await.unwrap().len(), 2);
        let importing = jobs(&db, Some("importing"), 10).await.unwrap();
        assert_eq!(importing.len(), 1);
        assert_eq!(importing[0].id, a.id);
        assert!(!claim_import(&db, a.id).await.unwrap());
        set_status(&db, a.id, "review", None).await.unwrap();
        assert!(claim_import(&db, a.id).await.unwrap());
        assert!(!claim_import(&db, a.id).await.unwrap());
    }

    #[tokio::test]
    async fn conversations_show_the_latest_message_and_unread_count() {
        let db = Db::memory().await;
        for (peer, out, body, at) in [
            ("ann", false, "first", "2026-10-01T10:00:00+00:00"),
            ("ann", false, "second", "2026-10-01T11:00:00.5+00:00"),
            ("ann", true, "reply", "2026-10-01T11:00:00+00:00"),
            ("bob", true, "hello", "2026-10-01T09:00:00+00:00"),
        ] {
            sqlx::query("INSERT INTO messages (account, peer, outgoing, body, at) VALUES ('me', ?1, ?2, ?3, ?4)")
                .bind(peer)
                .bind(out)
                .bind(body)
                .bind(at)
                .execute(&db.write)
                .await
                .unwrap();
        }
        let rows: Vec<(String, String, bool, chrono::DateTime<chrono::Utc>, i64)> =
            sqlx::query_as(crate::social::CONVERSATIONS)
                .bind("me")
                .fetch_all(&db.read)
                .await
                .unwrap();
        let view: Vec<_> = rows
            .iter()
            .map(|r| (r.0.as_str(), r.1.as_str(), r.4))
            .collect();
        assert_eq!(view, [("ann", "second", 2), ("bob", "hello", 0)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_database_reads_what_its_writer_wrote() {
        let path = std::env::temp_dir().join(format!("slsk-{}.db", Uuid::new_v4()));
        let db = Db::open(&path).await.unwrap();
        let j = job("a", "downloading");
        insert_job(&db, &j, &[]).await.unwrap();
        assert_eq!(super::job(&db, j.id).await.unwrap().unwrap().title, "a");
        assert!(
            sqlx::query("DELETE FROM jobs")
                .execute(&db.read)
                .await
                .is_err(),
            "readers are read-only"
        );
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db.read)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
