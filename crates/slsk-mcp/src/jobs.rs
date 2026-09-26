//! Jobs: a folder (usually an album) on its way from a peer into the library.
//!
//! downloading → importing → imported, or → review when the tagger cannot
//! match it confidently, or → failed. A folder whose peer fails falls back to
//! the next folder that matched the same request before giving up.
//!
//! One loop watches every active job, reading progress from the engine's
//! memory and writing to the database only when a file changes state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use slsk_engine::Engine;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::Directory;
use sqlx::PgPool;
use sqlx::types::Json;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::db::{self, Alternate, Job, JobFile, Source};
use crate::folders::Folder;
use crate::session::Session;

/// Times a job's failed files are asked for again from the same peer before
/// another source is tried.
const RETRY_ROUNDS: u32 = 2;

pub struct Jobs {
    db: PgPool,
    session: Arc<Session>,
    staging: PathBuf,
    complete: PathBuf,
    /// Spectrograms, one directory per job.
    spectrograms: PathBuf,
    tagger: Arc<sift::Importer>,
    /// Imports touch the library tree; one at a time keeps two albums from
    /// racing for the same destination.
    import_lock: Mutex<()>,
    /// Imports put off while MusicBrainz is unavailable: when to try again,
    /// and the release a person chose, if any.
    deferred: std::sync::Mutex<HashMap<Uuid, (tokio::time::Instant, Option<String>)>>,
    /// Rounds of retrying a job's failed files from the same peer, and when
    /// the next may start.
    retried: std::sync::Mutex<HashMap<Uuid, (u32, tokio::time::Instant)>>,
    wake: Notify,
}

impl Jobs {
    pub fn new(
        db: PgPool,
        session: Arc<Session>,
        staging: PathBuf,
        complete: PathBuf,
        spectrograms: PathBuf,
        tagger: Arc<sift::Importer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            session,
            staging,
            complete,
            spectrograms,
            tagger,
            import_lock: Mutex::new(()),
            deferred: Default::default(),
            retried: Default::default(),
            wake: Notify::new(),
        })
    }

    /// Where a job's files arrive: by id, since it holds `.part` files and
    /// nobody browses it.
    fn dir(&self, id: Uuid) -> PathBuf {
        self.staging.join(id.to_string())
    }

    /// Where a finished job waits: named for the album so it can be found by
    /// eye, with the id's first block so two jobs for one album do not meet.
    fn complete_dir(&self, job: &Job) -> PathBuf {
        let name: String = job
            .title
            .chars()
            .map(|c| {
                if matches!(
                    c,
                    '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                ) {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        let name = name.trim().trim_start_matches('.');
        let short = &job.id.to_string()[..8];
        self.complete.join(format!(
            "{} [{short}]",
            if name.is_empty() { "job" } else { name }
        ))
    }

    /// Queue `folder` and everything under it. The peer is asked for the
    /// complete listing, since search results only carry files whose paths
    /// matched the query — a cover image or a second disc can be missing.
    pub async fn from_folder(
        self: &Arc<Self>,
        folder: &Folder,
        title: Option<String>,
        alternates: Vec<Alternate>,
    ) -> Result<Job> {
        let engine = self.session.require()?.clone();
        let files = self
            .listing(&engine, &folder.username, &folder.remote_path, Some(folder))
            .await?;
        let id = Uuid::new_v4();
        let title = title.unwrap_or_else(|| {
            folder
                .path
                .rsplit('\\')
                .next()
                .unwrap_or(&folder.path)
                .to_string()
        });
        let job = Job {
            id,
            account: engine.username(),
            title,
            source: Json(Source::Soulseek {
                username: folder.username.clone(),
                folder: folder.path.clone(),
                folder_raw: folder.remote_path.as_bytes().to_vec(),
            }),
            alternates: Json(alternates),
            status: "downloading".into(),
            error: None,
            import_log: None,
            candidates: None,
            library_path: None,
            analysis: None,
            approved: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let rows = self.rows(id, &folder.username, &files);
        db::insert_job(&self.db, &job, &rows).await?;
        self.start(&engine, id, &rows);
        self.wake.notify_one();
        Ok(job)
    }

    /// Individual files from one user, as a job of their own.
    pub async fn from_files(
        self: &Arc<Self>,
        username: &str,
        files: Vec<(RawStr, u64)>,
        title: String,
    ) -> Result<Job> {
        let engine = self.session.require()?.clone();
        let id = Uuid::new_v4();
        let listing: Vec<(RawStr, u64, String)> = files
            .into_iter()
            .map(|(r, s)| (r, s, String::new()))
            .collect();
        let job = Job {
            id,
            account: engine.username(),
            title,
            source: Json(Source::Files {
                username: username.to_string(),
            }),
            alternates: Json(Vec::new()),
            status: "downloading".into(),
            error: None,
            import_log: None,
            candidates: None,
            library_path: None,
            analysis: None,
            approved: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let rows = self.rows(id, username, &listing);
        db::insert_job(&self.db, &job, &rows).await?;
        self.start(&engine, id, &rows);
        self.wake.notify_one();
        Ok(job)
    }

    /// (remote path, size, subdirectory relative to the folder)
    async fn listing(
        &self,
        engine: &Engine,
        username: &str,
        folder: &RawStr,
        fallback: Option<&Folder>,
    ) -> Result<Vec<(RawStr, u64, String)>> {
        let root = folder.to_string_lossy();
        let from_dirs = |dirs: Vec<Directory>| -> Vec<(RawStr, u64, String)> {
            dirs.into_iter()
                .flat_map(|d| {
                    let dir = d.name.to_string_lossy();
                    let sub = dir
                        .strip_prefix(&root)
                        .unwrap_or("")
                        .trim_start_matches('\\')
                        .replace('\\', "/");
                    let prefix = d.name.clone();
                    d.files.into_iter().map(move |f| {
                        let mut full = prefix.as_bytes().to_vec();
                        full.push(b'\\');
                        full.extend_from_slice(f.name.as_bytes());
                        (RawStr(full.into()), f.size, sub.clone())
                    })
                })
                .collect()
        };
        match engine.folder_contents(username, folder).await {
            Ok(dirs) if dirs.iter().any(|d| !d.files.is_empty()) => Ok(from_dirs(dirs)),
            other => {
                if let Err(e) = other {
                    tracing::debug!(%username, error = %e, "folder listing failed; using search results");
                }
                let folder = fallback.context("the peer did not list the folder")?;
                // Disc folders are grouped under their album, so each file's
                // own directory still decides where it lands.
                Ok(folder
                    .files
                    .iter()
                    .map(|f| {
                        let path = f.remote.to_string_lossy();
                        let dir = path.rsplit_once('\\').map_or("", |(d, _)| d);
                        let sub = dir
                            .strip_prefix(&root)
                            .unwrap_or("")
                            .trim_start_matches('\\')
                            .replace('\\', "/");
                        (f.remote.clone(), f.size, sub)
                    })
                    .collect())
            }
        }
    }

    fn rows(&self, id: Uuid, username: &str, files: &[(RawStr, u64, String)]) -> Vec<JobFile> {
        files
            .iter()
            .map(|(remote, size, sub)| JobFile {
                job_id: id,
                peer: username.to_string(),
                remote: remote.as_bytes().to_vec(),
                size: *size as i64,
                subdir: sub.clone(),
                state: "queued".into(),
                error: None,
            })
            .collect()
    }

    fn dest(&self, f: &JobFile) -> PathBuf {
        let name = RawStr(f.remote.clone().into()).to_string_lossy();
        let base = name.rsplit('\\').next().unwrap_or(&name);
        // Peers control these names; nothing they send may climb out of the
        // job's directory.
        let clean = |s: &str| {
            s.replace(['/', '\0'], "_")
                .trim_start_matches('.')
                .to_string()
        };
        let mut path = self.dir(f.job_id);
        for part in f.subdir.split('/').filter(|p| !p.is_empty() && *p != "..") {
            path.push(clean(part));
        }
        path.push(clean(base));
        path
    }

    fn start(&self, engine: &Engine, _id: Uuid, rows: &[JobFile]) {
        for f in rows.iter().filter(|f| f.state != "completed") {
            engine.download(
                &f.peer,
                RawStr(f.remote.clone().into()),
                f.size as u64,
                self.dest(f),
            );
        }
    }

    /// Re-queue unfinished downloads after a restart. The engine resumes each
    /// from its `.part`.
    pub async fn resume(self: &Arc<Self>) -> Result<()> {
        let engine = self.session.require()?.clone();
        for job in db::jobs(&self.db, Some("downloading"), 10_000).await? {
            let rows = db::job_files(&self.db, job.id).await?;
            self.start(&engine, job.id, &rows);
        }
        for job in db::jobs(&self.db, Some("importing"), 10_000).await? {
            let jobs = self.clone();
            tokio::spawn(async move { jobs.import(job.id, None).await });
        }
        self.wake.notify_one();
        Ok(())
    }

    pub fn spawn(self: &Arc<Self>) {
        let jobs = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = jobs.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                if let Err(e) = jobs.tick().await {
                    tracing::warn!(error = %e, "job tick failed");
                }
            }
        });
    }

    async fn tick(self: &Arc<Self>) -> Result<()> {
        let due: Vec<_> = {
            let mut deferred = self.deferred.lock().expect("deferred imports");
            let now = tokio::time::Instant::now();
            let ids: Vec<Uuid> = deferred
                .iter()
                .filter(|(_, (at, _))| *at <= now)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| deferred.remove(&id).map(|(_, r)| (id, r)))
                .collect()
        };
        for (id, release) in due {
            let jobs = self.clone();
            tokio::spawn(async move { jobs.import(id, release).await });
        }
        let Some(engine) = self.session.engine().cloned() else {
            return Ok(());
        };
        for job in db::jobs(&self.db, Some("downloading"), 10_000).await? {
            let rows = db::job_files(&self.db, job.id).await?;
            let mut states: HashMap<&str, usize> = HashMap::new();
            for f in &rows {
                let remote = RawStr(f.remote.clone().into());
                let (state, error) = match engine.download_view(&f.peer, &remote) {
                    Some(v) => (v.state.to_string(), v.error),
                    // Not in the engine: completed before a restart, or lost.
                    None if f.state == "completed" => ("completed".to_string(), None),
                    None => {
                        engine.download(&f.peer, remote, f.size as u64, self.dest(f));
                        ("queued".to_string(), None)
                    }
                };
                if state != f.state {
                    db::set_file_state(
                        &self.db,
                        job.id,
                        &f.peer,
                        &f.remote,
                        &state,
                        error.as_deref(),
                    )
                    .await?;
                }
                *states
                    .entry(match state.as_str() {
                        "completed" => "completed",
                        "failed" | "cancelled" => "failed",
                        _ => "active",
                    })
                    .or_default() += 1;
            }
            let (done, failed, active) = (
                states.get("completed"),
                states.get("failed"),
                states.get("active"),
            );
            if active.is_some() {
                continue;
            }
            if failed.is_none() && done.is_some() {
                self.retried.lock().expect("retried").remove(&job.id);
                db::set_status(&self.db, job.id, "importing", None).await?;
                let jobs = self.clone();
                tokio::spawn(async move { jobs.import(job.id, None).await });
            } else if failed.is_some() {
                // Most failures are a peer dropping off for a moment. Asking
                // the same peer again, spaced out, resumes from the partial
                // files; a new source would start the album from nothing.
                let now = tokio::time::Instant::now();
                let round = {
                    let mut retried = self.retried.lock().expect("retried");
                    let (rounds, next) = retried.entry(job.id).or_insert((0, now));
                    if *rounds >= RETRY_ROUNDS {
                        retried.remove(&job.id);
                        None
                    } else if now < *next {
                        Some(false)
                    } else {
                        *rounds += 1;
                        *next = now + Duration::from_secs(60 * u64::from(*rounds));
                        Some(true)
                    }
                };
                match round {
                    Some(true) => {
                        for f in rows
                            .iter()
                            .filter(|f| matches!(f.state.as_str(), "failed" | "cancelled"))
                        {
                            let remote = RawStr(f.remote.clone().into());
                            if !engine.retry_download(&f.peer, &remote) {
                                engine.download(&f.peer, remote, f.size as u64, self.dest(f));
                            }
                        }
                    }
                    Some(false) => {}
                    None => self.fall_back(&engine, &job, &rows).await?,
                }
            }
        }
        Ok(())
    }

    /// The folder failed; move to the next candidate, or give up.
    async fn fall_back(
        self: &Arc<Self>,
        engine: &Engine,
        job: &Job,
        rows: &[JobFile],
    ) -> Result<()> {
        let mut alternates = job.alternates.0.clone();
        let errors: Vec<String> = rows.iter().filter_map(|f| f.error.clone()).collect();
        for f in rows {
            engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
        }
        while !alternates.is_empty() {
            let alt = alternates.remove(0);
            let folder = RawStr(alt.folder_raw.clone().into());
            match self.listing(engine, &alt.username, &folder, None).await {
                Ok(files) if !files.is_empty() => {
                    tracing::info!(job = %job.id, user = %alt.username, ?errors, "falling back to another source");
                    let _ = tokio::fs::remove_dir_all(self.dir(job.id)).await;
                    let source = Source::Soulseek {
                        username: alt.username.clone(),
                        folder: alt.folder.clone(),
                        folder_raw: alt.folder_raw.clone(),
                    };
                    let rows = self.rows(job.id, &alt.username, &files);
                    db::replace_files(&self.db, job.id, &source, &alternates, &rows).await?;
                    self.start(engine, job.id, &rows);
                    return Ok(());
                }
                _ => continue,
            }
        }
        let error = errors
            .first()
            .cloned()
            .unwrap_or_else(|| "download failed".into());
        db::set_status(&self.db, job.id, "failed", Some(&error)).await?;
        Ok(())
    }

    /// Tag and move into the library. `release` forces a specific
    /// MusicBrainz release, which is how a job in review is resolved.
    pub async fn import(self: &Arc<Self>, id: Uuid, release: Option<String>) -> Result<()> {
        let _lock = self.import_lock.lock().await;
        let job = db::job(&self.db, id).await?.context("no such job")?;
        // Out of incomplete/ and into complete/ first, so what is left for a
        // person to look at is in one findable place whatever happens next.
        // A fresh download replaces what an earlier attempt left there.
        let dir = self.complete_dir(&job);
        let incoming = self.dir(id);
        if tokio::fs::try_exists(&incoming).await.unwrap_or(false) {
            if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
            if let Err(e) = tokio::fs::rename(&incoming, &dir).await {
                tracing::warn!(error = %e, "could not move {} to {}", incoming.display(), dir.display());
            }
        }
        let dir = if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            dir
        } else {
            incoming
        };
        db::set_status(&self.db, id, "importing", None).await?;
        // A person who looked at the analysis and approved, or who named the
        // release, has already decided; everyone else gets the check.
        if !job.approved && release.is_none() {
            let tracks = self.analyse(id, &dir).await;
            db::set_analysis(&self.db, id, &tracks).await?;
            let (verdict, confidence) = crate::analysis::album_verdict(&tracks);
            if matches!(
                verdict,
                crate::analysis::Verdict::Lossy | crate::analysis::Verdict::Upsampled
            ) && confidence >= 0.8
            {
                let flagged: Vec<_> = tracks.iter().filter(|t| t.verdict == verdict).collect();
                let example = flagged
                    .iter()
                    .find_map(|t| t.estimate.clone())
                    .unwrap_or_default();
                let reason = format!(
                    "{} of {} lossless-labelled tracks look {}: {example}. Check the spectrograms; approveJob imports it anyway.",
                    flagged.len(),
                    tracks.len(),
                    if verdict == crate::analysis::Verdict::Lossy {
                        "like a lossy source"
                    } else {
                        "upsampled"
                    },
                );
                db::set_status(&self.db, id, "suspect", Some(&reason)).await?;
                return Ok(());
            }
        }
        let outcome = self.tagger.import(&dir, release.as_deref()).await;
        match outcome {
            Ok(sift::Outcome::Imported { dir: path, log, .. }) => {
                db::set_import_log(&self.db, id, &log).await?;
                db::set_imported(&self.db, id, &path.to_string_lossy()).await?;
                let _ = tokio::fs::remove_dir_all(&dir).await;
                if let Some(engine) = self.session.engine().cloned() {
                    tokio::spawn(async move { engine.rescan().await });
                }
            }
            Ok(sift::Outcome::Review {
                reason,
                candidates,
                log,
            }) => {
                db::set_import_log(&self.db, id, &log).await?;
                db::set_review(&self.db, id, &reason, &candidates).await?;
            }
            // MusicBrainz being busy says nothing about the files, so the job
            // waits rather than asking a person to retry it.
            Err(e) if e.is_transient() => {
                db::set_status(
                    &self.db,
                    id,
                    "importing",
                    Some(&format!("{e:#}; trying again in 5 minutes")),
                )
                .await?;
                self.deferred.lock().expect("deferred imports").insert(
                    id,
                    (
                        tokio::time::Instant::now() + Duration::from_secs(300),
                        release,
                    ),
                );
            }
            Err(e) => {
                db::set_status(&self.db, id, "failed", Some(&format!("{e:#}"))).await?;
            }
        }
        Ok(())
    }

    /// Spectral analysis of every lossless file in `dir`, a few at a time:
    /// decoding is CPU-bound, and the engine serving uploads on the same node
    /// matters more than this finishing quickly.
    async fn analyse(
        &self,
        id: Uuid,
        dir: &std::path::Path,
    ) -> Vec<crate::analysis::TrackAnalysis> {
        const LOSSLESS: &[&str] = &["flac", "wav", "aiff", "aif", "alac", "m4a"];
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(mut rd) = tokio::fs::read_dir(&d).await else {
                continue;
            };
            while let Ok(Some(e)) = rd.next_entry().await {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| LOSSLESS.contains(&x.to_ascii_lowercase().as_str()))
                {
                    files.push(p);
                }
            }
        }
        files.sort();
        let out_dir = self.spectrograms.join(id.to_string());
        let limit = Arc::new(tokio::sync::Semaphore::new(3));
        let tasks: Vec<_> = files
            .into_iter()
            .enumerate()
            .map(|(i, path)| {
                let (limit, png) = (limit.clone(), out_dir.join(format!("{i:02}.png")));
                tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await;
                    tokio::task::spawn_blocking(move || crate::analysis::analyse(&path, Some(&png)))
                        .await
                        .ok()?
                        .ok()
                })
            })
            .collect();
        let mut tracks = Vec::new();
        for t in tasks {
            if let Ok(Some(a)) = t.await {
                tracks.push(a);
            }
        }
        tracks
    }

    /// Import a suspect job anyway.
    pub async fn approve(self: &Arc<Self>, id: Uuid) -> Result<()> {
        db::set_approved(&self.db, id).await?;
        self.import(id, None).await
    }

    pub fn spectrogram(&self, id: Uuid, n: u32) -> PathBuf {
        self.spectrograms
            .join(id.to_string())
            .join(format!("{n:02}.png"))
    }

    pub async fn cancel(&self, id: Uuid) -> Result<()> {
        let engine = self.session.require()?;
        for f in db::job_files(&self.db, id).await? {
            engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
        }
        db::set_status(&self.db, id, "cancelled", None).await?;
        Ok(())
    }

    /// Retry a failed or cancelled job from its current source.
    pub async fn retry(self: &Arc<Self>, id: Uuid) -> Result<()> {
        let engine = self.session.require()?.clone();
        let job = db::job(&self.db, id).await?.context("no such job")?;
        // Everything arrived and only the import went wrong: import again,
        // rather than fetch the album a second time. In the background, so
        // the caller is not held for the length of an import.
        let downloaded = tokio::fs::try_exists(self.complete_dir(&job))
            .await
            .unwrap_or(false);
        match job.status.as_str() {
            "review" | "suspect" => {}
            "failed" if downloaded => {}
            "failed" | "cancelled" => return self.redownload(&engine, id).await,
            s => bail!("job is {s}; only failed, cancelled, review and suspect jobs retry"),
        }
        db::set_status(&self.db, id, "importing", None).await?;
        let jobs = self.clone();
        tokio::spawn(async move {
            if let Err(e) = jobs.import(id, None).await {
                tracing::warn!(%id, error = %e, "retried import failed");
            }
        });
        Ok(())
    }

    async fn redownload(&self, engine: &Engine, id: Uuid) -> Result<()> {
        let rows = db::job_files(&self.db, id).await?;
        for f in &rows {
            let remote = RawStr(f.remote.clone().into());
            if !engine.retry_download(&f.peer, &remote) {
                engine.download(&f.peer, remote, f.size as u64, self.dest(f));
            }
        }
        db::set_status(&self.db, id, "downloading", None).await?;
        self.wake.notify_one();
        Ok(())
    }

    pub async fn remove(&self, id: Uuid) -> Result<()> {
        if let Some(engine) = self.session.engine() {
            for f in db::job_files(&self.db, id).await? {
                engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
            }
        }
        let _ = tokio::fs::remove_dir_all(self.dir(id)).await;
        if let Some(job) = db::job(&self.db, id).await? {
            let _ = tokio::fs::remove_dir_all(self.complete_dir(&job)).await;
        }
        db::delete_job(&self.db, id).await?;
        Ok(())
    }
}
