//! SQLite: accounts, jobs, bans, messages and the rest of what must survive a
//! restart. Transfer progress is not here: it lives in the engine's memory and
//! is read from there.
//!
//! One file in the state directory, reached through a pool of rusqlite
//! connections as koan and sift reach theirs. rusqlite blocks, so every
//! statement runs on tokio's blocking pool through [`Db::run`] and never
//! occupies an async worker. WAL lets reads run beside a write; writers wait
//! on one another through `busy_timeout`, and a transaction that writes opens
//! with `BEGIN IMMEDIATE` so it waits for the lock up front instead of failing
//! when it tries to upgrade a read.
//!
//! Encodings are fixed by the rows already stored: UUIDs as 16-byte blobs
//! (rusqlite's `uuid` feature), timestamps as RFC 3339 text with a `T`, which
//! is what the schema's `strftime` defaults write and what [`ts`] produces,
//! and JSON as text through [`Json`]. rusqlite's own chrono encoding uses a
//! space instead of the `T`, which would sort and compare wrongly against the
//! stored values, so timestamps are bound with [`ts`] and read with
//! [`ts_from`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use rusqlite::Result;

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
         WHERE cause IS NOT NULL AND at >= ?1 ORDER BY at DESC";

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

/// Jobs still in hand, newest first: everything but those that landed or
/// were cancelled. The condition is the `jobs_active` index's, word for
/// word, which is what lets SQLite use it.
pub(crate) const ACTIVE_JOBS: &str = "SELECT * FROM jobs WHERE status NOT IN ('imported', 'cancelled') \
         ORDER BY created_at DESC LIMIT ?1";
/// Jobs that landed or were cancelled, newest first.
pub(crate) const SETTLED_JOBS: &str = "SELECT * FROM jobs WHERE status IN ('imported', 'cancelled') ORDER BY created_at DESC LIMIT ?1";

/// The current time as the schema stores it, for use inside SQL.
macro_rules! now {
    () => {
        "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')"
    };
}
pub(crate) use now;

/// Migrations in order; `PRAGMA user_version` counts those applied.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_active_jobs.sql"),
    include_str!("../migrations/0003_import_how.sql"),
    include_str!("../migrations/0004_profiles.sql"),
    include_str!("../migrations/0005_end_sessions.sql"),
    include_str!("../migrations/0006_waiting.sql"),
];

/// How many idle connections to keep. More than the concurrency the service
/// reaches, so the steady state never opens one; a burst past it closes the
/// extras when they come back.
const KEEP_IDLE: usize = 8;

#[derive(Clone)]
pub struct Db(Arc<Pool>);

struct Pool {
    path: PathBuf,
    idle: parking_lot::Mutex<Vec<Connection>>,
}

impl Db {
    /// Open (or create) the database at `path` and bring its schema up to
    /// date.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        let db = tokio::task::spawn_blocking(move || -> anyhow::Result<Self> {
            let mut conn = Connection::open(&path)?;
            configure(&conn)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            migrate(&mut conn)?;
            optimize(&conn);
            Ok(Self(Arc::new(Pool {
                path,
                idle: parking_lot::Mutex::new(vec![conn]),
            })))
        })
        .await??;
        Ok(db)
    }

    /// A fresh database in a temporary file, for tests: a file rather than
    /// `:memory:` so every pooled connection sees the same data.
    #[cfg(test)]
    pub async fn memory() -> Self {
        let path = std::env::temp_dir().join(format!("slsk-test-{}.db", Uuid::new_v4()));
        Self::open(&path).await.expect("test database")
    }

    /// Run `f` on a pooled connection, on the blocking pool.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.0.clone();
        match tokio::task::spawn_blocking(move || {
            let mut conn = match pool.idle.lock().pop() {
                Some(c) => c,
                None => {
                    let c = Connection::open(&pool.path)?;
                    configure(&c)?;
                    c
                }
            };
            let out = f(&mut conn);
            let mut idle = pool.idle.lock();
            if idle.len() < KEEP_IDLE {
                idle.push(conn);
            }
            out
        })
        .await
        {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    /// Refresh the planner's statistics where they have drifted. Bounded, so
    /// it costs milliseconds; without statistics SQLite can pick an index that
    /// only supplies the ordering and read the whole table.
    pub async fn optimize(&self) {
        let _ = self
            .run(|c| {
                optimize(c);
                Ok(())
            })
            .await;
    }
}

/// Settings every connection needs; WAL is a property of the file and is set
/// once, at open.
fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA temp_store = MEMORY;",
    )
}

fn optimize(conn: &Connection) {
    if let Err(e) = conn.execute_batch("PRAGMA analysis_limit = 400; PRAGMA optimize = 0x10002;") {
        tracing::warn!(error = %e, "database optimize failed");
    }
}

/// Apply the migrations `user_version` says are missing. A database created
/// while the service used sqlx records its migrations in `_sqlx_migrations`
/// instead, and those count as applied.
fn migrate(conn: &mut Connection) -> Result<()> {
    let mut applied: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if applied == 0 {
        let legacy: Option<i64> = conn
            .query_row(
                "SELECT max(version) FROM _sqlx_migrations WHERE success",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None)
            .flatten();
        applied = legacy.unwrap_or(0);
    }
    let applied = usize::try_from(applied).unwrap_or(0);
    if applied > MIGRATIONS.len() {
        return newer_schema(applied);
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64)
}

/// A database migrated by a newer build: refuse it rather than run against a
/// schema this one does not know.
fn newer_schema(found: usize) -> Result<()> {
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
        Some(format!(
            "database schema version {found} is newer than this build's {}",
            MIGRATIONS.len()
        )),
    ))
}

/// A timestamp as the schema stores it, for binding.
pub fn ts(t: &chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
}

/// A stored timestamp, read back.
pub fn ts_from(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// A JSON column, read and written through serde.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Json<T>(pub T);

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
        serde_json::to_string(&self.0)
            .map(ToSqlOutput::from)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
}

impl<T: serde::de::DeserializeOwned> FromSql for Json<T> {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let bytes = match v {
            ValueRef::Text(b) | ValueRef::Blob(b) => b,
            _ => return Err(FromSqlError::InvalidType),
        };
        serde_json::from_slice(bytes)
            .map(Json)
            .map_err(FromSqlError::other)
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

pub async fn active_account(db: &Db) -> Result<Option<(String, String)>> {
    db.run(|c| {
        c.query_row(
            "SELECT username, sealed_password FROM accounts WHERE active",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })
    .await
}

pub async fn save_account(db: &Db, username: &str, sealed: &str) -> Result<()> {
    let (username, sealed) = (username.to_string(), sealed.to_string());
    db.run(move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE accounts SET active = FALSE WHERE active AND username <> ?1",
            params![username],
        )?;
        tx.execute(
            concat!("INSERT INTO accounts (username, sealed_password, active) VALUES (?1, ?2, TRUE)
         ON CONFLICT (username) DO UPDATE SET sealed_password = EXCLUDED.sealed_password, active = TRUE, updated_at = ", now!(), ""),
            params![username, sealed],
        )?;
        tx.commit()
    })
    .await
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
    /// A file fetched over HTTP: an album as a zip, or a single track.
    Url { url: String },
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

#[derive(Debug, Clone)]
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
    /// How its latest import was asked to decide the album.
    /// `None` for one written by a build whose `How` no longer reads.
    pub import_how: Option<crate::jobs::How>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Job {
    /// A new job, downloading from `source`.
    pub fn new(account: String, title: String, source: Source, alternates: Vec<Alternate>) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: Uuid::new_v4(),
            account,
            title,
            source: Json(source),
            alternates: Json(alternates),
            status: "downloading".into(),
            error: None,
            import_log: None,
            candidates: None,
            library_path: None,
            analysis: None,
            approved: false,
            as_is_blocker: None,
            replaces: false,
            import_how: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn from_row(r: &rusqlite::Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            account: r.get("account")?,
            title: r.get("title")?,
            source: r.get("source")?,
            alternates: r.get("alternates")?,
            status: r.get("status")?,
            error: r.get("error")?,
            import_log: r.get("import_log")?,
            candidates: r.get("candidates")?,
            library_path: r.get("library_path")?,
            analysis: r.get("analysis")?,
            approved: r.get("approved")?,
            as_is_blocker: r.get("as_is_blocker")?,
            replaces: r.get("replaces")?,
            import_how: r
                .get::<_, Option<String>>("import_how")?
                .and_then(|s| match serde_json::from_str(&s) {
                    Ok(how) => Some(how),
                    Err(e) => {
                        tracing::warn!(error = %e, "a job's import decision no longer reads; matching afresh");
                        None
                    }
                }),
            created_at: ts_from(&r.get::<_, String>("created_at")?)?,
            updated_at: ts_from(&r.get::<_, String>("updated_at")?)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct JobFile {
    pub job_id: Uuid,
    pub peer: String,
    pub remote: Vec<u8>,
    pub size: i64,
    pub subdir: String,
    pub state: String,
    pub error: Option<String>,
}

impl JobFile {
    pub fn from_row(r: &rusqlite::Row<'_>) -> Result<Self> {
        Ok(Self {
            job_id: r.get("job_id")?,
            peer: r.get("peer")?,
            remote: r.get("remote")?,
            size: r.get("size")?,
            subdir: r.get("subdir")?,
            state: r.get("state")?,
            error: r.get("error")?,
        })
    }
}

fn insert_files(c: &Connection, files: &[JobFile]) -> Result<()> {
    let mut s = c.prepare_cached(
        "INSERT INTO job_files (job_id, peer, remote, size, subdir) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for f in files {
        s.execute(params![f.job_id, f.peer, f.remote, f.size, f.subdir])?;
    }
    Ok(())
}

pub async fn insert_job(db: &Db, job: &Job, files: &[JobFile]) -> Result<()> {
    let (job, files) = (job.clone(), files.to_vec());
    db.run(move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO jobs (id, account, title, source, alternates, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![job.id, job.account, job.title, job.source, job.alternates, job.status],
        )?;
        insert_files(&tx, &files)?;
        tx.commit()
    })
    .await
}

pub async fn set_replaces(db: &Db, id: Uuid) -> Result<()> {
    db.run(move |c| {
        c.execute("UPDATE jobs SET replaces = true WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await
}

/// Move the job onto another source and download it, unless it was
/// cancelled, removed or imported meanwhile. Whether it moved.
pub async fn replace_files(
    db: &Db,
    job: Uuid,
    source: &Source,
    alternates: &[Alternate],
    files: &[JobFile],
) -> Result<bool> {
    let (source, alternates, files) = (source.clone(), alternates.to_vec(), files.to_vec());
    db.run(move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let moved = tx.execute(
            concat!("UPDATE jobs SET source = ?2, alternates = ?3, status = 'downloading', error = NULL, updated_at = ", now!(),
                " WHERE id = ?1 AND status IN ('downloading', 'review', 'suspect', 'failed')"),
            params![job, Json(&source), Json(&alternates)],
        )? > 0;
        if !moved {
            return Ok(false);
        }
        tx.execute("DELETE FROM job_files WHERE job_id = ?1", params![job])?;
        insert_files(&tx, &files)?;
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn jobs(db: &Db, status: Option<&str>, limit: i64) -> Result<Vec<Job>> {
    let status = status.map(str::to_string);
    db.run(move |c| match status {
        Some(status) => c
            .prepare_cached(JOBS_OF_STATUS)?
            .query_map(params![status, limit], Job::from_row)?
            .collect(),
        None => c
            .prepare_cached(JOBS)?
            .query_map(params![limit], Job::from_row)?
            .collect(),
    })
    .await
}

/// The newest `limit` jobs still in hand, and the newest `limit` of those
/// that landed or were cancelled.
pub async fn job_board(db: &Db, limit: i64) -> Result<(Vec<Job>, Vec<Job>)> {
    db.run(move |c| {
        let active = c
            .prepare_cached(ACTIVE_JOBS)?
            .query_map(params![limit], Job::from_row)?
            .collect::<Result<_>>()?;
        let settled = c
            .prepare_cached(SETTLED_JOBS)?
            .query_map(params![limit], Job::from_row)?
            .collect::<Result<_>>()?;
        Ok((active, settled))
    })
    .await
}

pub async fn job(db: &Db, id: Uuid) -> Result<Option<Job>> {
    db.run(move |c| {
        c.prepare_cached("SELECT * FROM jobs WHERE id = ?1")?
            .query_row(params![id], Job::from_row)
            .optional()
    })
    .await
}

/// The newest job a grab for `query` made in the last day, unless it failed:
/// asking again for what is already on its way is the same request.
pub async fn live_grab(db: &Db, query: &str) -> Result<Option<Job>> {
    let query = query.trim().to_string();
    let since = ts(&(chrono::Utc::now() - chrono::Duration::days(1)));
    db.run(move |c| {
        c.prepare_cached(LIVE_GRAB)?
            .query_row(params![query, since], Job::from_row)
            .optional()
    })
    .await
}

pub async fn job_files(db: &Db, id: Uuid) -> Result<Vec<JobFile>> {
    db.run(move |c| {
        c.prepare_cached(JOB_FILES)?
            .query_map(params![id], JobFile::from_row)?
            .collect()
    })
    .await
}

/// The files of every job in `ids`, by job, in one query.
pub async fn files_of_jobs(
    db: &Db,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<JobFile>>> {
    let ids = id_list(ids);
    let rows: Vec<JobFile> = db
        .run(move |c| {
            c.prepare_cached(FILES_OF_JOBS)?
                .query_map(params![ids], JobFile::from_row)?
                .collect()
        })
        .await?;
    let mut by_job: std::collections::HashMap<Uuid, Vec<JobFile>> = Default::default();
    for f in rows {
        by_job.entry(f.job_id).or_default().push(f);
    }
    Ok(by_job)
}

pub async fn set_title(db: &Db, id: Uuid, title: &str) -> Result<()> {
    let title = title.to_string();
    db.run(move |c| {
        c.execute(
            "UPDATE jobs SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_status(db: &Db, id: Uuid, status: &str, error: Option<&str>) -> Result<()> {
    let (status, error) = (status.to_string(), error.map(str::to_string));
    db.run(move |c| {
        c.execute(
            concat!(
                "UPDATE jobs SET status = ?2, error = ?3, updated_at = ",
                now!(),
                " WHERE id = ?1"
            ),
            params![id, status, error],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_import_log(db: &Db, id: Uuid, log: &str) -> Result<()> {
    let log = log.to_string();
    db.run(move |c| {
        c.execute(
            concat!(
                "UPDATE jobs SET import_log = ?2, updated_at = ",
                now!(),
                " WHERE id = ?1"
            ),
            params![id, log],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_file_state(
    db: &Db,
    id: Uuid,
    peer: &str,
    remote: &[u8],
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    let (peer, remote, state, error) = (
        peer.to_string(),
        remote.to_vec(),
        state.to_string(),
        error.map(str::to_string),
    );
    // A file's bytes count toward the lifetime total once, on the way into
    // completed. The transaction holds the write lock from its start, so
    // nothing changes the row between reading and updating it.
    db.run(move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prev: Option<(String, i64)> = tx
            .prepare_cached(FILE_STATE)?
            .query_row(params![id, peer, remote], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        tx.execute(
            "UPDATE job_files SET state = ?4, error = ?5 WHERE job_id = ?1 AND peer = ?2 AND remote = ?3",
            params![id, peer, remote, state, error],
        )?;
        if let Some((was, size)) = prev
            && state == "completed"
            && was != "completed"
        {
            add_total(&tx, "downloaded_bytes", size)?;
        }
        tx.commit()
    })
    .await
}

fn add_total(c: &Connection, name: &str, by: i64) -> Result<()> {
    c.prepare_cached(
        "INSERT INTO totals (name, value) VALUES (?1, ?2) \
         ON CONFLICT (name) DO UPDATE SET value = value + excluded.value",
    )?
    .execute(params![name, by])?;
    Ok(())
}

pub async fn delete_job(db: &Db, id: Uuid) -> Result<()> {
    db.run(move |c| {
        c.execute("DELETE FROM jobs WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await
}

/// The account's description and picture, if it has saved one.
pub async fn profile(db: &Db, account: &str) -> Result<Option<(String, Option<Vec<u8>>)>> {
    let account = account.to_string();
    db.run(move |c| {
        c.prepare_cached("SELECT description, picture FROM profiles WHERE account = ?1")?
            .query_row(params![account], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    })
    .await
}

pub async fn save_profile(
    db: &Db,
    account: &str,
    description: &str,
    picture: Option<&[u8]>,
) -> Result<()> {
    let (account, description, picture) = (
        account.to_string(),
        description.to_string(),
        picture.map(<[u8]>::to_vec),
    );
    db.run(move |c| {
        c.execute(
            "INSERT INTO profiles (account, description, picture) VALUES (?1, ?2, ?3) \
             ON CONFLICT (account) DO UPDATE SET description = EXCLUDED.description, picture = EXCLUDED.picture",
            params![account, description, picture],
        )?;
        Ok(())
    })
    .await
}

pub async fn bans(db: &Db, account: &str) -> Result<Vec<String>> {
    let account = account.to_string();
    db.run(move |c| {
        c.prepare_cached("SELECT username FROM bans WHERE account = ?1 ORDER BY username")?
            .query_map(params![account], |r| r.get(0))?
            .collect()
    })
    .await
}

pub async fn set_ban(db: &Db, account: &str, username: &str, banned: bool) -> Result<()> {
    let q = if banned {
        "INSERT INTO bans (account, username) VALUES (?1, ?2) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM bans WHERE account = ?1 AND username = ?2"
    };
    let (account, username) = (account.to_string(), username.to_string());
    db.run(move |c| {
        c.execute(q, params![account, username])?;
        Ok(())
    })
    .await
}

/// Append an outcome to the job's history, in `job_events`.
pub async fn record_event(
    db: &Db,
    job_id: Uuid,
    title: &str,
    peer: Option<&str>,
    outcome: &str,
    cause: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    let (title, peer, outcome, cause, detail) = (
        title.to_string(),
        peer.map(str::to_string),
        outcome.to_string(),
        cause.map(str::to_string),
        detail.map(str::to_string),
    );
    db.run(move |c| {
        c.prepare_cached(
            "INSERT INTO job_events (job_id, title, version, outcome, cause, detail, peer) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![
            job_id,
            title,
            env!("CARGO_PKG_VERSION"),
            outcome,
            cause,
            detail,
            peer
        ])?;
        Ok(())
    })
    .await
}

#[derive(Debug, Clone)]
pub struct JobEvent {
    pub job_id: Uuid,
    pub title: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub version: String,
    pub outcome: String,
    pub cause: Option<String>,
    pub detail: Option<String>,
}

impl JobEvent {
    pub fn from_row(r: &rusqlite::Row<'_>) -> Result<Self> {
        Ok(Self {
            job_id: r.get("job_id")?,
            title: r.get("title")?,
            at: ts_from(&r.get::<_, String>("at")?)?,
            version: r.get("version")?,
            outcome: r.get("outcome")?,
            cause: r.get("cause")?,
            detail: r.get("detail")?,
        })
    }
}

/// Events with a cause since `since`, newest first.
pub async fn events_since(db: &Db, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<JobEvent>> {
    let since = ts(&since);
    db.run(move |c| {
        c.prepare_cached(EVENTS_SINCE)?
            .query_map(params![since], JobEvent::from_row)?
            .collect()
    })
    .await
}

/// Every event ever recorded, by outcome and cause: monotonic, so it serves
/// as a counter.
pub async fn event_counts(db: &Db) -> Result<Vec<(String, Option<String>, i64)>> {
    db.run(|c| {
        c.prepare_cached("SELECT outcome, cause, count(*) FROM job_events GROUP BY 1, 2")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    })
    .await
}

/// Peers that stalled a download in the last day. Events from before the
/// peer was recorded name it through their job's source.
pub async fn stalled_peers(db: &Db) -> Result<Vec<String>> {
    let since = ts(&(chrono::Utc::now() - chrono::Duration::days(1)));
    db.run(move |c| {
        c.prepare_cached(STALLED_PEERS)?
            .query_map(params![since], |r| r.get(0))?
            .collect()
    })
    .await
}

pub async fn set_imported(db: &Db, id: Uuid, path: &str) -> Result<()> {
    let path = path.to_string();
    db.run(move |c| {
        c.execute(
            concat!("UPDATE jobs SET status = 'imported', library_path = ?2, error = NULL, candidates = NULL, updated_at = ", now!(), " WHERE id = ?1"),
            params![id, path],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_review(
    db: &Db,
    id: Uuid,
    reason: &str,
    candidates: &[sift::Candidate],
) -> Result<()> {
    let (reason, candidates) = (reason.to_string(), Json(candidates.to_vec()));
    db.run(move |c| {
        c.execute(
            concat!(
                "UPDATE jobs SET status = 'review', error = ?2, candidates = ?3, updated_at = ",
                now!(),
                " WHERE id = ?1"
            ),
            params![id, reason, candidates],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_analysis(
    db: &Db,
    id: Uuid,
    analysis: &[crate::analysis::TrackAnalysis],
) -> Result<()> {
    let analysis = Json(analysis.to_vec());
    db.run(move |c| {
        c.execute(
            concat!(
                "UPDATE jobs SET analysis = ?2, updated_at = ",
                now!(),
                " WHERE id = ?1"
            ),
            params![id, analysis],
        )?;
        Ok(())
    })
    .await
}

/// The states a job waiting on a person's decision is in.
pub const DECIDED: &[&str] = &["review", "suspect", "failed"];

/// Move a job from one of `from` to `importing`, recording how the import
/// is to decide the album. One statement, so two requests for the same job
/// cannot both succeed, and a restart finds the decision with the status.
/// The count of put-off tries carries over only from `waiting`: a person's
/// retry or decision starts it again.
pub async fn claim_import(
    db: &Db,
    id: Uuid,
    from: &[&str],
    how: &crate::jobs::How,
) -> Result<bool> {
    let (from, how) = (serde_json::to_string(from).unwrap_or_default(), how.clone());
    db.run(move |c| {
        Ok(c.query_row(
            concat!(
                "UPDATE jobs SET status = 'importing', error = NULL, import_how = ?3, \
                 defer_tries = CASE WHEN status = 'waiting' THEN defer_tries ELSE 0 END, updated_at = ",
                now!(),
                " WHERE id = ?1 AND status IN (SELECT value FROM json_each(?2)) RETURNING id"
            ),
            params![id, from, Json(&how)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

/// Put an `importing` job's import off until a metadata service is back:
/// `waiting`, with `error` saying why. Refused, and `None`, once it has been
/// put off `limit` times or when the job has moved on; otherwise the number
/// of times so far.
pub async fn defer_import(db: &Db, id: Uuid, error: &str, limit: u32) -> Result<Option<u32>> {
    let error = error.to_string();
    db.run(move |c| {
        Ok(c.query_row(
            concat!(
                "UPDATE jobs SET status = 'waiting', error = ?2, defer_tries = defer_tries + 1, updated_at = ",
                now!(),
                " WHERE id = ?1 AND status = 'importing' AND defer_tries < ?3 RETURNING defer_tries"
            ),
            params![id, error, limit],
            |r| r.get(0),
        )
        .optional()?)
    })
    .await
}

/// Jobs `waiting` for at least `secs`, due another try.
pub async fn due_waiting(db: &Db, secs: u64) -> Result<Vec<Job>> {
    let since = format!("-{secs} seconds");
    db.run(move |c| {
        c.prepare_cached(
            "SELECT * FROM jobs WHERE status = 'waiting' \
             AND updated_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', ?1)",
        )?
        .query_map([since], Job::from_row)?
        .collect::<Result<_>>()
    })
    .await
}

/// Move a job from one of `from` to `to` with `error`, in one statement, so
/// a job that has moved on meanwhile (cancelled, removed) is left alone.
/// Whether it moved.
pub async fn move_status(
    db: &Db,
    id: Uuid,
    from: &[&str],
    to: &str,
    error: Option<&str>,
) -> Result<bool> {
    let (from, to, error) = (
        serde_json::to_string(from).unwrap_or_default(),
        to.to_string(),
        error.map(str::to_string),
    );
    db.run(move |c| {
        Ok(c.query_row(
            concat!(
                "UPDATE jobs SET status = ?2, error = ?4, updated_at = ",
                now!(),
                " WHERE id = ?1 AND status IN (SELECT value FROM json_each(?3)) RETURNING id"
            ),
            params![id, to, from, error],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

pub async fn set_as_is_blocker(db: &Db, id: Uuid, blocker: &str) -> Result<()> {
    let blocker = blocker.to_string();
    db.run(move |c| {
        c.execute(
            "UPDATE jobs SET as_is_blocker = ?2 WHERE id = ?1",
            params![id, blocker],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_approved(db: &Db, id: Uuid) -> Result<()> {
    db.run(move |c| {
        c.execute(
            concat!(
                "UPDATE jobs SET approved = TRUE, updated_at = ",
                now!(),
                " WHERE id = ?1"
            ),
            params![id],
        )?;
        Ok(())
    })
    .await
}

#[derive(Clone)]
pub struct UploadRow {
    pub username: String,
    pub filename: String,
    pub size: i64,
    pub bytes: i64,
    pub state: String,
    pub seconds: Option<f64>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

impl UploadRow {
    pub fn from_row(r: &rusqlite::Row<'_>) -> Result<Self> {
        Ok(Self {
            username: r.get("username")?,
            filename: r.get("filename")?,
            size: r.get("size")?,
            bytes: r.get("bytes")?,
            state: r.get("state")?,
            seconds: r.get("seconds")?,
            finished_at: ts_from(&r.get::<_, String>("finished_at")?)?,
        })
    }
}

/// A finished upload, with how long it was seen sending when known.
pub async fn record_upload(
    db: &Db,
    t: &slsk_engine::TransferView,
    seconds: Option<f64>,
) -> Result<()> {
    let (username, filename, size, bytes, state, error) = (
        t.username.clone(),
        t.filename.to_string_lossy(),
        t.size as i64,
        t.bytes as i64,
        t.state,
        t.error.clone(),
    );
    db.run(move |c| {
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.prepare_cached(
            "INSERT INTO uploads (username, filename, size, bytes, state, error, seconds) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![username, filename, size, bytes, state, error, seconds])?;
        add_total(&tx, "uploaded_bytes", bytes)?;
        add_total(&tx, &format!("uploads_{state}"), 1)?;
        if state == "completed" {
            tx.prepare_cached(concat!(
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
            ))?
            .execute(params![username, bytes])?;
        }
        tx.commit()
    })
    .await
}

pub async fn recent_uploads(db: &Db, limit: i64) -> Result<Vec<UploadRow>> {
    db.run(move |c| {
        c.prepare_cached(RECENT_UPLOADS)?
            .query_map(params![limit], UploadRow::from_row)?
            .collect()
    })
    .await
}

/// Completed uploads since `since`: how many, and how many bytes.
pub async fn upload_totals(db: &Db, since: chrono::DateTime<chrono::Utc>) -> Result<(i64, i64)> {
    let since = ts(&since);
    db.run(move |c| {
        c.prepare_cached(UPLOAD_TOTALS)?
            .query_row(params![since], |r| Ok((r.get(0)?, r.get(1)?)))
    })
    .await
}

/// History older than `days` goes; it is for looking back weeks, not years.
pub async fn prune_uploads(db: &Db, days: i32) -> Result<u64> {
    let before = ts(&(chrono::Utc::now() - chrono::Duration::days(i64::from(days))));
    db.run(move |c| {
        Ok(c.execute(
            "DELETE FROM uploads WHERE finished_at < ?1",
            params![before],
        )? as u64)
    })
    .await
}

/// Lifetime totals by name: `uploaded_bytes`, `downloaded_bytes`, and
/// `uploads_<state>`.
pub async fn totals(db: &Db) -> Result<Vec<(String, i64)>> {
    db.run(|c| {
        c.prepare_cached("SELECT name, value FROM totals ORDER BY name")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    })
    .await
}

/// Distinct users an upload has finished to: in the last day, the last week,
/// and ever.
pub async fn served_user_counts(db: &Db) -> Result<(i64, i64, i64)> {
    let now = chrono::Utc::now();
    let (day, week) = (
        ts(&(now - chrono::Duration::days(1))),
        ts(&(now - chrono::Duration::days(7))),
    );
    db.run(move |c| {
        c.query_row(
            "SELECT \
                 COUNT(*) FILTER (WHERE last_served >= ?1), \
                 COUNT(*) FILTER (WHERE last_served >= ?2), \
                 COUNT(*) \
             FROM served_users",
            params![day, week],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    })
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
            ACTIVE_JOBS,
            SETTLED_JOBS,
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
            let plan: Vec<String> = db
                .run(move |c| {
                    let mut s = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let nulls = std::iter::repeat_n(rusqlite::types::Null, s.parameter_count());
                    s.query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))?
                        .collect()
                })
                .await
                .unwrap();
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
        let mut job = Job::new(
            "me".into(),
            title.into(),
            Source::Files {
                username: "peer".into(),
            },
            Vec::new(),
        );
        job.status = status.into();
        job
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
        let old = ts(&(chrono::Utc::now() - chrono::Duration::hours(25)));
        db.run(move |c| c.execute("UPDATE jobs SET created_at = ?1", params![old]))
            .await
            .unwrap();
        assert!(live_grab(&db, "burial untrue").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_put_off_import_waits_counts_its_tries_and_stops_at_the_limit() {
        let db = Db::memory().await;
        let job = job("a", "importing");
        insert_job(&db, &job, &[]).await.unwrap();
        assert_eq!(defer_import(&db, job.id, "down", 2).await.unwrap(), Some(1));
        assert_eq!(
            super::job(&db, job.id).await.unwrap().unwrap().status,
            "waiting"
        );
        // Not due until the wait has passed; due at once with no wait.
        assert!(due_waiting(&db, 300).await.unwrap().is_empty());
        assert_eq!(due_waiting(&db, 0).await.unwrap().len(), 1);
        // Back to importing from waiting keeps the count.
        let how = crate::jobs::How::Match(None);
        assert!(claim_import(&db, job.id, &["waiting"], &how).await.unwrap());
        assert_eq!(defer_import(&db, job.id, "down", 2).await.unwrap(), Some(2));
        assert!(claim_import(&db, job.id, &["waiting"], &how).await.unwrap());
        assert_eq!(defer_import(&db, job.id, "down", 2).await.unwrap(), None);
        // A person's retry from failed starts the count again.
        set_status(&db, job.id, "failed", None).await.unwrap();
        assert!(claim_import(&db, job.id, &["failed"], &how).await.unwrap());
        assert_eq!(defer_import(&db, job.id, "down", 2).await.unwrap(), Some(1));
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
        let (from, how) = (DECIDED, &crate::jobs::How::Match(None));
        assert!(!claim_import(&db, a.id, from, how).await.unwrap());
        set_status(&db, a.id, "review", None).await.unwrap();
        assert!(claim_import(&db, a.id, from, how).await.unwrap());
        assert!(!claim_import(&db, a.id, from, how).await.unwrap());
    }

    #[tokio::test]
    async fn conversations_show_the_latest_message_and_unread_count() {
        let db = Db::memory().await;
        let rows = db
            .run(|c| {
                for (peer, out, body, at) in [
                    ("ann", false, "first", "2026-10-01T10:00:00+00:00"),
                    ("ann", false, "second", "2026-10-01T11:00:00.5+00:00"),
                    ("ann", true, "reply", "2026-10-01T11:00:00+00:00"),
                    ("bob", true, "hello", "2026-10-01T09:00:00+00:00"),
                ] {
                    c.execute(
                        "INSERT INTO messages (account, peer, outgoing, body, at) VALUES ('me', ?1, ?2, ?3, ?4)",
                        params![peer, out, body, at],
                    )?;
                }
                c.prepare(crate::social::CONVERSATIONS)?
                    .query_map(params!["me"], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .unwrap();
        let view: Vec<_> = rows
            .iter()
            .map(|r| (r.0.as_str(), r.1.as_str(), r.2))
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
        let mode: String = db
            .run(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// A database sqlx migrated keeps its schema: its recorded migrations
    /// count as applied rather than running again over existing tables.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_database_migrated_by_sqlx_opens_as_is() {
        let path = std::env::temp_dir().join(format!("slsk-{}.db", Uuid::new_v4()));
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute_batch(
                "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, success BOOLEAN NOT NULL); \
                 INSERT INTO _sqlx_migrations VALUES (1, TRUE);",
            )
            .unwrap();
        }
        let db = Db::open(&path).await.unwrap();
        let version: i64 = db
            .run(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .await
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn a_status_moves_only_from_the_states_named() {
        let db = Db::memory().await;
        let j = job("a", "imported");
        insert_job(&db, &j, &[]).await.unwrap();
        assert!(
            !move_status(&db, j.id, &["downloading"], "cancelled", None)
                .await
                .unwrap()
        );
        assert_eq!(
            super::job(&db, j.id).await.unwrap().unwrap().status,
            "imported"
        );
        assert!(
            move_status(&db, j.id, &["downloading", "imported"], "removing", None)
                .await
                .unwrap()
        );
        assert_eq!(
            super::job(&db, j.id).await.unwrap().unwrap().status,
            "removing"
        );
    }

    #[tokio::test]
    async fn a_new_source_does_not_revive_a_cancelled_job() {
        let db = Db::memory().await;
        let j = job("a", "cancelled");
        insert_job(&db, &j, &[file(&j, "01.flac", 10)])
            .await
            .unwrap();
        let source = Source::Files {
            username: "other".into(),
        };
        let moved = replace_files(&db, j.id, &source, &[], &[file(&j, "02.flac", 10)])
            .await
            .unwrap();
        assert!(!moved);
        assert_eq!(
            super::job(&db, j.id).await.unwrap().unwrap().status,
            "cancelled"
        );
        assert_eq!(job_files(&db, j.id).await.unwrap().len(), 1);

        let k = job("b", "review");
        insert_job(&db, &k, &[]).await.unwrap();
        assert!(
            replace_files(&db, k.id, &source, &[], &[file(&k, "01.flac", 10)])
                .await
                .unwrap()
        );
        assert_eq!(
            super::job(&db, k.id).await.unwrap().unwrap().status,
            "downloading"
        );
    }

    #[tokio::test]
    async fn an_import_keeps_how_it_was_asked() {
        let db = Db::memory().await;
        let j = job("a", "review");
        insert_job(&db, &j, &[]).await.unwrap();
        let how = crate::jobs::How::Match(Some("release".into()));
        assert!(claim_import(&db, j.id, DECIDED, &how).await.unwrap());
        let read = super::job(&db, j.id).await.unwrap().unwrap().import_how;
        assert!(matches!(read, Some(crate::jobs::How::Match(Some(r))) if r == "release"));
        // A decision written by a build whose `How` differed reads as none,
        // rather than failing every query that lists the job.
        db.run(move |c| {
            c.execute(
                "UPDATE jobs SET import_how = '{\"Gone\":1}' WHERE id = ?1",
                params![j.id],
            )
        })
        .await
        .unwrap();
        assert!(
            super::job(&db, j.id)
                .await
                .unwrap()
                .unwrap()
                .import_how
                .is_none()
        );
    }
}
