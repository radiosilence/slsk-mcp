//! Postgres: accounts, jobs, bans. Transfer progress is not here — it lives in
//! the engine's memory and is read from there; the database holds only what
//! must survive a restart.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::types::Json;
use uuid::Uuid;

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub async fn active_account(db: &PgPool) -> sqlx::Result<Option<(String, String)>> {
    sqlx::query_as("SELECT username, sealed_password FROM accounts WHERE active")
        .fetch_optional(db)
        .await
}

pub async fn save_account(db: &PgPool, username: &str, sealed: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE accounts SET active = FALSE WHERE active AND username <> $1")
        .bind(username)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO accounts (username, sealed_password, active) VALUES ($1, $2, TRUE)
         ON CONFLICT (username) DO UPDATE SET sealed_password = EXCLUDED.sealed_password, active = TRUE, updated_at = now()",
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

pub async fn insert_job(db: &PgPool, job: &Job, files: &[JobFile]) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO jobs (id, account, title, source, alternates, status) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(job.id)
        .bind(&job.account)
        .bind(&job.title)
        .bind(&job.source)
        .bind(&job.alternates)
        .bind(&job.status)
        .execute(&mut *tx)
        .await?;
    for f in files {
        sqlx::query("INSERT INTO job_files (job_id, peer, remote, size, subdir) VALUES ($1, $2, $3, $4, $5)")
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

pub async fn replace_files(
    db: &PgPool,
    job: Uuid,
    source: &Source,
    alternates: &[Alternate],
    files: &[JobFile],
) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM job_files WHERE job_id = $1")
        .bind(job)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE jobs SET source = $2, alternates = $3, status = 'downloading', error = NULL, updated_at = now() WHERE id = $1")
        .bind(job)
        .bind(Json(source))
        .bind(Json(alternates))
        .execute(&mut *tx)
        .await?;
    for f in files {
        sqlx::query("INSERT INTO job_files (job_id, peer, remote, size, subdir) VALUES ($1, $2, $3, $4, $5)")
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

pub async fn jobs(db: &PgPool, status: Option<&str>, limit: i64) -> sqlx::Result<Vec<Job>> {
    sqlx::query_as("SELECT * FROM jobs WHERE ($1::text IS NULL OR status = $1) ORDER BY created_at DESC LIMIT $2")
        .bind(status)
        .bind(limit)
        .fetch_all(db)
        .await
}

pub async fn job(db: &PgPool, id: Uuid) -> sqlx::Result<Option<Job>> {
    sqlx::query_as("SELECT * FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn job_files(db: &PgPool, id: Uuid) -> sqlx::Result<Vec<JobFile>> {
    sqlx::query_as("SELECT * FROM job_files WHERE job_id = $1 ORDER BY subdir, remote")
        .bind(id)
        .fetch_all(db)
        .await
}

pub async fn set_status(
    db: &PgPool,
    id: Uuid,
    status: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET status = $2, error = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_import_log(db: &PgPool, id: Uuid, log: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET import_log = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(log)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_file_state(
    db: &PgPool,
    id: Uuid,
    peer: &str,
    remote: &[u8],
    state: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE job_files SET state = $4, error = $5 WHERE job_id = $1 AND peer = $2 AND remote = $3")
        .bind(id)
        .bind(peer)
        .bind(remote)
        .bind(state)
        .bind(error)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn delete_job(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn bans(db: &PgPool, account: &str) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT username FROM bans WHERE account = $1 ORDER BY username")
        .bind(account)
        .fetch_all(db)
        .await
}

pub async fn set_ban(db: &PgPool, account: &str, username: &str, banned: bool) -> sqlx::Result<()> {
    let q = if banned {
        "INSERT INTO bans (account, username) VALUES ($1, $2) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM bans WHERE account = $1 AND username = $2"
    };
    sqlx::query(q)
        .bind(account)
        .bind(username)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_imported(db: &PgPool, id: Uuid, path: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET status = 'imported', library_path = $2, error = NULL, candidates = NULL, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(path)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_review(
    db: &PgPool,
    id: Uuid,
    reason: &str,
    candidates: &[sift::Candidate],
) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET status = 'review', error = $2, candidates = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(reason)
        .bind(Json(candidates))
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_analysis(
    db: &PgPool,
    id: Uuid,
    analysis: &[crate::analysis::TrackAnalysis],
) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET analysis = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(Json(analysis))
        .execute(db)
        .await
        .map(|_| ())
}

/// Move a job to `importing` if it is waiting on a decision. One statement,
/// so two requests for the same job cannot both succeed.
pub async fn claim_import(db: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    sqlx::query(
        "UPDATE jobs SET status = 'importing', error = NULL, updated_at = now() \
         WHERE id = $1 AND status IN ('review', 'suspect', 'failed') RETURNING id",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map(|row| row.is_some())
}

pub async fn set_approved(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE jobs SET approved = TRUE, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map(|_| ())
}
