//! Jobs: a folder (usually an album) on its way from a peer into the library.
//!
//! downloading → importing → imported, or → review when the tagger cannot
//! match it confidently, or → failed. A folder whose peer fails falls back to
//! the next folder that matched the same request before giving up.
//!
//! A job can also start from a URL rather than a peer: the file is fetched
//! over HTTP, unpacked if it is a zip, and goes on to import as a download
//! does. It has no other source to fall back to.
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
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::db::{self, Alternate, Job, JobFile, Json, Source};
use crate::folders::Folder;
use crate::session::Session;

/// Why a job did not land cleanly: a fixed set, so outcomes can be counted
/// and a cause traced to a fix.
pub mod cause {
    /// Every file failed from the peer, after retries.
    pub const PEER_FAILED: &str = "peer_failed";
    /// A fetch from a URL failed or was refused.
    pub const URL_FAILED: &str = "url_failed";
    /// Queued with the peer, nothing received for the stall limit.
    pub const STALLED_PEER: &str = "stalled_peer";
    /// Files that do not parse as audio.
    pub const CORRUPT_COPY: &str = "corrupt_copy";
    /// No audio files where the download should be: the peer's folder held
    /// none, or they went missing here, which is a bug worth chasing.
    pub const NO_AUDIO: &str = "no_audio";
    /// A person or assistant asked for another copy.
    pub const REQUESTED: &str = "requested";
    /// MusicBrainz offered nothing.
    pub const NO_CANDIDATES: &str = "no_candidates";
    /// The closest release lacks tracks the files have, or the files lack
    /// tracks it has.
    pub const INCOMPLETE: &str = "incomplete";
    pub const EXTRA_FILES: &str = "extra_files";
    /// Complete, but not close enough to apply unasked.
    pub const WEAK_MATCH: &str = "weak_match";
    pub const LOSSY_SOURCE: &str = "lossy_source";
    pub const UPSAMPLED: &str = "upsampled";
    pub const MB_UNAVAILABLE: &str = "mb_unavailable";
    pub const IMPORT_ERROR: &str = "import_error";
    /// Import as-is refused: the files' tags do not describe one album.
    pub const UNTAGGED: &str = "untagged";
}

/// How an import decides what the album is.
#[derive(Debug, Clone)]
enum How {
    /// Against MusicBrainz; with a release id, that release.
    Match(Option<String>),
    /// By the files' own tags, with any corrections.
    AsIs(sift::Edits),
}

/// Times a job's failed files are asked for again from the same peer before
/// another source is tried.
const RETRY_ROUNDS: u32 = 2;

/// How long a download may go without a single byte before another source
/// is tried.
const STALL: Duration = Duration::from_secs(20 * 60);
/// How long a search for another copy collects responses.
const SEARCH_WAIT: Duration = Duration::from_secs(15);

/// How long after a start before stalls are judged. The clock counts from
/// the job row, so a restart does not forgive a stalled peer; this keeps the
/// first tick from judging every peer, and every fallback's folder listing,
/// before the session has logged in and reached anyone.
const STARTUP_GRACE: Duration = Duration::from_secs(3 * 60);

/// The most a fetch from a URL may download, and the most its zip may
/// unpack to: well above a hi-res album, well below a full disk.
const URL_MAX_BYTES: u64 = 8 << 30;
/// How long a fetch from a URL may go without a byte before it fails.
const URL_IDLE: Duration = Duration::from_secs(60);
/// How long a fetch from a URL may take in all.
const URL_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// A fetch from a URL in progress: bytes received, and the size the server
/// announced (0 when it did not).
#[derive(Default)]
struct Fetched {
    received: std::sync::atomic::AtomicU64,
    size: std::sync::atomic::AtomicU64,
}

pub struct Jobs {
    db: crate::db::Db,
    session: Arc<Session>,
    staging: PathBuf,
    complete: PathBuf,
    /// Spectrograms, one directory per job.
    spectrograms: PathBuf,
    tagger: Arc<sift::Importer>,
    /// Its bin is where a copy a replacing import sets aside goes.
    library: Arc<crate::library::Library>,
    /// Imports touch the library tree; one at a time keeps two albums from
    /// racing for the same destination.
    import_lock: Mutex<()>,
    /// Imports put off while MusicBrainz is unavailable: when to try again,
    /// and the release a person chose, if any.
    deferred: std::sync::Mutex<HashMap<Uuid, (tokio::time::Instant, Option<String>)>>,
    /// Rounds of retrying a job's failed files from the same peer, and when
    /// the next may start.
    retried: std::sync::Mutex<HashMap<Uuid, (u32, tokio::time::Instant)>>,
    /// Set on shutdown: imports still waiting for the lock leave the job
    /// `importing` for the next start to resume.
    closing: std::sync::atomic::AtomicBool,
    /// Per downloading job, when bytes last arrived and how many had then.
    /// A file finishing and the rest never starting is a stall, so this
    /// tracks progress rather than whether anything ever arrived.
    stalled: std::sync::Mutex<HashMap<Uuid, (tokio::time::Instant, u64)>>,
    /// Per downloading job, its best place in the peer's upload queue while
    /// nothing is transferring, for the UI.
    places: std::sync::Mutex<HashMap<Uuid, u32>>,
    /// When this process started: stalls are not judged until the session
    /// has had time to reach the network again.
    started: tokio::time::Instant,
    /// Peers that queued a download and sent nothing, and when: grab ranks
    /// their folders last for a day, since cancelling our queue with them
    /// makes them look idle again.
    stalled_peers: std::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// Fetches from URLs in progress, to follow and to stop.
    fetches: std::sync::Mutex<HashMap<Uuid, (tokio::task::AbortHandle, Arc<Fetched>)>>,
    http: reqwest::Client,
    wake: Notify,
}

impl Jobs {
    pub fn new(
        db: crate::db::Db,
        session: Arc<Session>,
        staging: PathBuf,
        complete: PathBuf,
        spectrograms: PathBuf,
        tagger: Arc<sift::Importer>,
        library: Arc<crate::library::Library>,
    ) -> Arc<Self> {
        Arc::new(Self {
            library,
            db,
            session,
            staging,
            complete,
            spectrograms,
            tagger,
            import_lock: Mutex::new(()),
            deferred: Default::default(),
            retried: Default::default(),
            closing: Default::default(),
            stalled: Default::default(),
            places: Default::default(),
            started: tokio::time::Instant::now(),
            stalled_peers: Default::default(),
            fetches: Default::default(),
            http: reqwest::Client::builder()
                .user_agent(concat!("slsk-mcp/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(30))
                .read_timeout(URL_IDLE)
                .timeout(URL_TIMEOUT)
                .build()
                .expect("HTTP client"),
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
            .listing(
                &engine,
                &folder.username,
                &folder.remote_path,
                &folder
                    .files
                    .iter()
                    .map(|f| (f.remote.clone(), f.size))
                    .collect::<Vec<_>>(),
            )
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
            as_is_blocker: None,
            replaces: false,
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
        let listing = files
            .into_iter()
            .map(|(r, s)| (r, s, String::new()))
            .collect();
        self.from_listing(username, listing, title).await
    }

    /// Files already known from a share listing, each with its
    /// subdirectory relative to the album, so disc folders stay apart.
    pub async fn from_listing(
        self: &Arc<Self>,
        username: &str,
        listing: Vec<(RawStr, u64, String)>,
        title: String,
    ) -> Result<Job> {
        let engine = self.session.require()?.clone();
        let id = Uuid::new_v4();
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
            as_is_blocker: None,
            replaces: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let rows = self.rows(id, username, &listing);
        db::insert_job(&self.db, &job, &rows).await?;
        self.start(&engine, id, &rows);
        self.wake.notify_one();
        Ok(job)
    }

    /// Fetch an album from a URL: a zip of it, as Bandcamp serves a
    /// purchase, or a single audio file.
    pub async fn from_url(self: &Arc<Self>, url: &str) -> Result<Job> {
        let url = check_url(url)?;
        let id = Uuid::new_v4();
        let job = Job {
            id,
            account: self
                .session
                .engine()
                .map(|e| e.username())
                .unwrap_or_default(),
            // Until the server names the file.
            title: url.host_str().unwrap_or_default().to_string(),
            source: Json(Source::Url {
                url: url.to_string(),
            }),
            alternates: Json(Vec::new()),
            status: "downloading".into(),
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
        };
        db::insert_job(&self.db, &job, &[]).await?;
        self.fetch(id, url.to_string());
        Ok(job)
    }

    /// Where a fetch from a URL lands before it is unpacked: beside the
    /// job's directory, so nothing in the archive can name it.
    fn fetch_file(&self, id: Uuid) -> PathBuf {
        self.staging.join(format!("{id}.fetch"))
    }

    /// Bytes received and announced by a fetch from a URL in progress.
    pub fn fetched(&self, id: Uuid) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        self.fetches
            .lock()
            .expect("fetches")
            .get(&id)
            .map(|(_, f)| (f.received.load(Relaxed), f.size.load(Relaxed)))
    }

    /// Fetch a URL job's file in the background, then import it.
    fn fetch(self: &Arc<Self>, id: Uuid, url: String) {
        let fetched = Arc::new(Fetched::default());
        // Held across the spawn, so the task cannot finish and remove its
        // entry before it is inserted.
        let mut fetches = self.fetches.lock().expect("fetches");
        let (jobs, progress) = (self.clone(), fetched.clone());
        let task = tokio::spawn(async move {
            let outcome = jobs.download_url(id, &url, &progress).await;
            jobs.fetches.lock().expect("fetches").remove(&id);
            let _ = tokio::fs::remove_file(jobs.fetch_file(id)).await;
            let to = if outcome.is_ok() {
                "importing"
            } else {
                "failed"
            };
            match (
                outcome,
                db::move_status(&jobs.db, id, &["downloading"], to).await,
            ) {
                (Ok(()), Ok(true)) => {
                    if let Err(e) = jobs.import(id, None).await {
                        tracing::warn!(%id, error = %e, "import failed");
                    }
                }
                (Err(e), Ok(true)) => {
                    let reason = format!("{e:#}");
                    tracing::info!(%id, host = %host(&url), error = %reason, "fetch failed");
                    if let Err(e) = db::set_status(&jobs.db, id, "failed", Some(&reason)).await {
                        tracing::warn!(%id, error = %e, "could not record a failed fetch");
                    }
                    if let Ok(Some(job)) = db::job(&jobs.db, id).await {
                        jobs.event(&job, "failed", Some(cause::URL_FAILED), Some(&reason))
                            .await;
                    }
                }
                // Cancelled or removed while it ran.
                (_, Ok(false)) => {}
                (_, Err(e)) => tracing::warn!(%id, error = %e, "could not record a fetch"),
            }
        });
        fetches.insert(id, (task.abort_handle(), fetched));
    }

    /// Download `url` into the job's directory, unpacking it if it is a zip,
    /// and name the job for the file.
    async fn download_url(&self, id: Uuid, url: &str, fetched: &Fetched) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        use tokio::io::AsyncWriteExt;
        let url = check_url(url)?;
        let host = url.host_str().unwrap_or_default().to_string();
        tracing::info!(job = %id, %host, "fetching from a URL");
        // Download links carry access tokens: errors name the host only.
        let mut resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| anyhow::anyhow!("could not fetch from {host}: {}", e.without_url()))?;
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v: &http::HeaderValue| v.to_str().ok())
                .map(str::to_string)
        };
        if header(http::header::CONTENT_TYPE).is_some_and(|t| t.starts_with("text/html")) {
            bail!("{host} answered with a web page, not a file: use the download link itself");
        }
        let disposition = header(http::header::CONTENT_DISPOSITION);
        let size = resp.content_length().unwrap_or(0);
        if size > URL_MAX_BYTES {
            bail!("{host} offers {size} bytes, more than the {URL_MAX_BYTES} a fetch may take");
        }
        fetched.size.store(size, Relaxed);
        let name = file_name(disposition.as_deref(), &url).unwrap_or_else(|| "download".into());
        let title = std::path::Path::new(&name)
            .file_stem()
            .map_or(name.clone(), |s| s.to_string_lossy().into_owned());
        db::set_title(&self.db, id, &title).await?;

        let dir = self.dir(id);
        // A retry starts again from nothing.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let part = self.fetch_file(id);
        let mut file = tokio::fs::File::create(&part).await?;
        let mut received = 0u64;
        let mut head = Vec::<u8>::with_capacity(4);
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("the fetch from {host} broke off: {}", e.without_url()))?
        {
            received += chunk.len() as u64;
            if received > URL_MAX_BYTES {
                bail!("{host} sent more than the {URL_MAX_BYTES} bytes a fetch may take");
            }
            if head.len() < 4 {
                head.extend(chunk.iter().take(4 - head.len()));
            }
            file.write_all(&chunk).await?;
            fetched.received.store(received, Relaxed);
        }
        file.flush().await?;
        drop(file);
        tracing::info!(job = %id, %host, bytes = received, "fetched");
        if head == b"PK\x03\x04" {
            tokio::task::spawn_blocking(move || unzip(&part, &dir, URL_MAX_BYTES)).await??;
        } else {
            tokio::fs::rename(&part, dir.join(&name)).await?;
        }
        Ok(())
    }

    /// (remote path, size, subdirectory relative to the folder)
    async fn listing(
        &self,
        engine: &Engine,
        username: &str,
        folder: &RawStr,
        fallback: &[(RawStr, u64)],
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
                anyhow::ensure!(!fallback.is_empty(), "the peer did not list the folder");
                // Disc folders are grouped under their album, so each file's
                // own directory still decides where it lands.
                Ok(fallback
                    .iter()
                    .map(|(remote, size)| {
                        let path = remote.to_string_lossy();
                        let dir = path.rsplit_once('\\').map_or("", |(d, _)| d);
                        let sub = dir
                            .strip_prefix(&root)
                            .unwrap_or("")
                            .trim_start_matches('\\')
                            .replace('\\', "/");
                        (remote.clone(), *size, sub)
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
        // Remembered across restarts: a peer that stalled us yesterday is
        // no more likely to send today because this process is new.
        let now = tokio::time::Instant::now();
        for peer in db::stalled_peers(&self.db).await.unwrap_or_default() {
            self.stalled_peers
                .lock()
                .expect("stalled peers")
                .insert(peer, now);
        }
        let (fetches, downloads): (Vec<_>, Vec<_>) =
            db::jobs(&self.db, Some("downloading"), 10_000)
                .await?
                .into_iter()
                .partition(|j| matches!(j.source.0, Source::Url { .. }));
        for job in fetches {
            if let Source::Url { url } = job.source.0 {
                self.fetch(job.id, url);
            }
        }
        let engine = self.session.require()?.clone();
        for job in downloads {
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

    /// The job's place in its peer's upload queue, while it is waiting in one.
    pub fn place(&self, id: Uuid) -> Option<u32> {
        self.places.lock().expect("places").get(&id).copied()
    }

    /// Peers that stalled a download in the last day.
    pub fn recently_stalled(&self) -> std::collections::HashSet<String> {
        let mut peers = self.stalled_peers.lock().expect("stalled peers");
        peers.retain(|_, at| at.elapsed() < Duration::from_secs(24 * 3600));
        peers.keys().cloned().collect()
    }

    /// Let an import in progress finish. Moving an album into the library is
    /// not atomic, and a process stopped halfway leaves it split between the
    /// staging folder and the library.
    pub async fn drain(&self) {
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _lock = self.import_lock.lock().await;
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
        // A fetch from a URL is followed by its own task.
        let jobs: Vec<_> = db::jobs(&self.db, Some("downloading"), 10_000)
            .await?
            .into_iter()
            .filter(|j| !matches!(j.source.0, Source::Url { .. }))
            .collect();
        let ids: Vec<Uuid> = jobs.iter().map(|j| j.id).collect();
        let mut files = db::files_of_jobs(&self.db, &ids).await?;
        // One job's failure is that job's: the rest still move.
        for job in jobs {
            let rows = files.remove(&job.id).unwrap_or_default();
            if let Err(e) = self.tick_job(&engine, &job, rows).await {
                tracing::warn!(job = %job.id, error = %e, "job tick failed");
            }
        }
        Ok(())
    }

    /// Follow one downloading job: record its files' states, and start its
    /// import, a retry or another source as they call for.
    async fn tick_job(
        self: &Arc<Self>,
        engine: &Engine,
        job: &Job,
        rows: Vec<JobFile>,
    ) -> Result<()> {
        if rows.is_empty() {
            db::set_status(&self.db, job.id, "failed", Some("no files to download")).await?;
            return Ok(());
        }
        let mut states: HashMap<&str, usize> = HashMap::new();
        let mut received = 0u64;
        let mut place: Option<u32> = None;
        let mut transferring = false;
        for f in &rows {
            let remote = RawStr(f.remote.clone().into());
            let (state, error) = match engine.download_view(&f.peer, &remote) {
                Some(v) => {
                    received += v.bytes;
                    transferring |= v.state == "transferring";
                    place = match (place, v.place) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    (v.state.to_string(), v.error)
                }
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
            // A peer that queues every file and never sends one (no free
            // slot for strangers, or a queue it never works through)
            // would hold the album forever while other sources sit
            // untried. A slow peer that is sending is left alone.
            {
                let mut places = self.places.lock().expect("places");
                match place.filter(|_| !transferring) {
                    Some(p) => places.insert(job.id, p),
                    None => places.remove(&job.id),
                };
            }
            let now = tokio::time::Instant::now();
            let since = {
                let mut stalled = self.stalled.lock().expect("stalled");
                let entry = stalled.entry(job.id).or_insert_with(|| {
                    // From when this source was taken on, which the job
                    // row records, so a restart does not forgive a peer
                    // that has sent nothing.
                    let waited = (chrono::Utc::now() - job.updated_at)
                        .to_std()
                        .unwrap_or_default();
                    (now.checked_sub(waited).unwrap_or(now), received)
                });
                if received > entry.1 {
                    *entry = (now, received);
                }
                entry.0
            };
            let stalled = now.duration_since(self.started) >= STARTUP_GRACE
                && now.duration_since(since) >= STALL;
            if stalled && job.alternates.0.is_empty() {
                // Nothing left to fall back to. Peers come and go, so a
                // copy that was not there when the job began may be now.
                // Restarting the clock spaces the searches a stall apart.
                self.stalled
                    .lock()
                    .expect("stalled")
                    .insert(job.id, (now, received));
                let (jobs, engine, job) = (self.clone(), engine.clone(), job.clone());
                tokio::spawn(async move {
                    if let Err(e) = jobs
                        .search_again(&engine, &job, cause::STALLED_PEER)
                        .await
                        .map(drop)
                    {
                        tracing::warn!(job = %job.id, error = %e, "search for another copy failed");
                    }
                });
            } else if stalled {
                self.stalled.lock().expect("stalled").remove(&job.id);
                tracing::info!(job = %job.id, "no data from the peer; trying another source");
                if let Some(peer) = rows.first().map(|r| r.peer.clone()) {
                    self.stalled_peers
                        .lock()
                        .expect("stalled peers")
                        .insert(peer, now);
                }
                self.fall_back(engine, job, &rows, cause::STALLED_PEER)
                    .await?;
            }
            return Ok(());
        }
        self.stalled.lock().expect("stalled").remove(&job.id);
        self.places.lock().expect("places").remove(&job.id);
        if failed.is_none() && done.is_some() {
            self.retried.lock().expect("retried").remove(&job.id);
            db::set_status(&self.db, job.id, "importing", None).await?;
            let (jobs, id) = (self.clone(), job.id);
            tokio::spawn(async move { jobs.import(id, None).await });
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
                // Out of stored fallbacks, as a folder picked by hand
                // always is: a peer that refuses (a daily file limit, a
                // ban) will not relent, but another may have the album.
                None if job.alternates.0.is_empty() => {
                    if !self.search_again(engine, job, cause::PEER_FAILED).await? {
                        self.fall_back(engine, job, &rows, cause::PEER_FAILED)
                            .await?
                    }
                }
                None => {
                    self.fall_back(engine, job, &rows, cause::PEER_FAILED)
                        .await?
                }
            }
        }
        Ok(())
    }

    /// The folder failed; move to the next candidate, or give up. `cause` is
    /// why this source is being left, for the job's history.
    async fn fall_back(
        self: &Arc<Self>,
        engine: &Engine,
        job: &Job,
        rows: &[JobFile],
        cause: &str,
    ) -> Result<()> {
        let mut alternates = job.alternates.0.clone();
        // A peer that stalled us recently goes last here as it does in grab:
        // the order was fixed when the job began, before it was known.
        let stalled = self.recently_stalled();
        alternates.sort_by_key(|a| stalled.contains(&a.username));
        let errors: Vec<String> = rows.iter().filter_map(|f| f.error.clone()).collect();
        let left = rows.first().map(|r| r.peer.clone()).unwrap_or_default();
        for f in rows {
            engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
        }
        while !alternates.is_empty() {
            let alt = alternates.remove(0);
            let folder = RawStr(alt.folder_raw.clone().into());
            let found: Vec<(RawStr, u64)> = alt
                .files
                .iter()
                .map(|(r, s)| (RawStr(r.clone().into()), *s))
                .collect();
            match self.listing(engine, &alt.username, &folder, &found).await {
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
                    let detail = format!("to {}; left {left}: {}", alt.username, errors.join("; "));
                    self.event(job, "fallback", Some(cause), Some(&detail))
                        .await;
                    return Ok(());
                }
                _ => continue,
            }
        }
        let error = errors.first().cloned().unwrap_or_else(|| {
            if cause == cause::STALLED_PEER {
                "the peer sent nothing for twenty minutes, and no other copy could be fetched"
                    .into()
            } else {
                "download failed, and no other copy could be fetched".into()
            }
        });
        db::set_status(&self.db, job.id, "failed", Some(&error)).await?;
        self.event(job, "failed", Some(cause), Some(&error)).await;
        Ok(())
    }

    /// Record an outcome in the job's history. A failure to record is
    /// logged, never allowed to fail the work it describes.
    async fn event(&self, job: &Job, outcome: &str, cause: Option<&str>, detail: Option<&str>) {
        let peer = match &job.source.0 {
            Source::Soulseek { username, .. } | Source::Files { username } => username.clone(),
            Source::Url { url } => host(url),
        };
        if let Err(e) = db::record_event(
            &self.db,
            job.id,
            &job.title,
            Some(&peer),
            outcome,
            cause,
            detail,
        )
        .await
        {
            tracing::warn!(job = %job.id, error = %e, "could not record job event");
        }
    }

    /// Tag and move into the library. `release` forces a specific
    /// MusicBrainz release, which is how a job in review is resolved.
    pub async fn import(self: &Arc<Self>, id: Uuid, release: Option<String>) -> Result<()> {
        self.run_import(id, How::Match(release)).await
    }

    /// In a task of its own, so a caller that stops waiting (a request its
    /// client dropped) cannot cancel an import halfway: the file moves would
    /// carry on in blocking threads, filing the album, and the job would
    /// never record it. The task also holds the import lock until the job is
    /// recorded, which is what `drain` waits on at shutdown.
    async fn run_import(self: &Arc<Self>, id: Uuid, how: How) -> Result<()> {
        let jobs = self.clone();
        tokio::spawn(async move { jobs.file(id, how).await }).await?
    }

    async fn file(self: &Arc<Self>, id: Uuid, how: How) -> Result<()> {
        let _lock = self.import_lock.lock().await;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let job = db::job(&self.db, id).await?.context("no such job")?;
        // Imports queue on the lock, so a second request for the same job
        // (a double tap, Retry beside Use) arrives after the first has
        // filed the album and cleared its folder. A job stopped or being
        // removed while one waited is not filed either.
        if matches!(job.status.as_str(), "imported" | "cancelled" | "removing") {
            return Ok(());
        }
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
        // A file that does not decode never goes in, whoever approved it: it
        // plays as noise, and no judgement about the release changes that.
        let tracks = self.analyse(id, &dir).await;
        db::set_analysis(&self.db, id, &tracks).await?;
        let damaged: Vec<_> = tracks.iter().filter(|t| t.damaged()).collect();
        if !damaged.is_empty() {
            let reason = format!(
                "{} of {} files do not decode ({}): a damaged copy",
                damaged.len(),
                tracks.len(),
                damaged
                    .iter()
                    .map(|t| match &t.error {
                        Some(e) => format!("{}: {e}", t.file),
                        None => format!("{}: {} bad frames", t.file, t.decode_errors),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            db::set_status(&self.db, id, "failed", Some(&reason)).await?;
            self.event(&job, "failed", Some(cause::CORRUPT_COPY), Some(&reason))
                .await;
            if let Err(e) = self.next_source(id, cause::CORRUPT_COPY).await {
                tracing::warn!(%id, error = %e, "damaged copy, and no other source: {reason}");
            }
            return Ok(());
        }
        // A person who looked at the analysis and approved, or who named the
        // release, has already decided; everyone else gets the check.
        // An album sent here as-is was checked on its way to review.
        if !job.approved && matches!(how, How::Match(None)) {
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
                let c = if verdict == crate::analysis::Verdict::Lossy {
                    cause::LOSSY_SOURCE
                } else {
                    cause::UPSAMPLED
                };
                self.event(&job, "suspect", Some(c), Some(&reason)).await;
                return Ok(());
            }
        }
        tracing::info!(%id, title = %job.title, dir = %dir.display(), "import started");
        let outcome = match &how {
            How::Match(release) if job.replaces => {
                self.tagger
                    .import_replacing(&dir, release.as_deref(), self.library.bin())
                    .await
            }
            How::Match(release) => self.tagger.import(&dir, release.as_deref()).await,
            How::AsIs(edits) => self.tagger.import_as_is(&dir, edits).await,
        };
        match &outcome {
            Ok(sift::Outcome::Imported { dir: path, .. }) => {
                tracing::info!(%id, library = %path.display(), "import finished: filed")
            }
            Ok(sift::Outcome::Review { .. }) => tracing::info!(%id, "import finished: review"),
            Err(e) => tracing::info!(%id, error = %e, "import finished: not filed"),
        }
        match outcome {
            Ok(sift::Outcome::Imported { dir: path, log, .. }) => {
                db::set_import_log(&self.db, id, &log).await?;
                db::set_imported(&self.db, id, &path.to_string_lossy()).await?;
                self.event(&job, "imported", None, Some(&path.to_string_lossy()))
                    .await;
                let _ = tokio::fs::remove_dir_all(&dir).await;
                // Gain, genres and lyrics, off the import path: an album is
                // playable as soon as it is filed, and these only add to it.
                self.library.changed();
                let (tagger, engine, library) = (
                    self.tagger.clone(),
                    self.session.engine().cloned(),
                    self.library.clone(),
                );
                tokio::spawn(async move {
                    let known = library.known_artists().await.unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "could not list the library's artists");
                        Default::default()
                    });
                    match tagger.enrich(&path, &known).await {
                        Ok(e) => tracing::info!(
                            album = %path.display(),
                            gain_db = ?e.gain_db,
                            genres = ?e.genres,
                            lyrics = e.lyrics,
                            artists = ?e.artists,
                            problems = ?e.problems,
                            "enriched"
                        ),
                        Err(e) => {
                            tracing::warn!(album = %path.display(), error = %e, "could not enrich")
                        }
                    }
                    library.changed();
                    if let Some(engine) = engine {
                        engine.rescan().await;
                    }
                });
            }
            Ok(sift::Outcome::Review {
                reason,
                candidates,
                log,
            }) => {
                db::set_import_log(&self.db, id, &log).await?;
                db::set_review(&self.db, id, &reason, &candidates).await?;
                let blocker = match self.tagger.check_as_is(&dir).await {
                    Ok(()) => String::new(),
                    Err(e) => format!("{e:#}"),
                };
                db::set_as_is_blocker(&self.db, id, &blocker).await?;
                let c = match candidates.first() {
                    None => cause::NO_CANDIDATES,
                    Some(b) if b.missing > 0 => cause::INCOMPLETE,
                    Some(b) if b.extra > 0 => cause::EXTRA_FILES,
                    Some(_) => cause::WEAK_MATCH,
                };
                self.event(&job, "review", Some(c), Some(&reason)).await;
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
                self.event(
                    &job,
                    "deferred",
                    Some(cause::MB_UNAVAILABLE),
                    Some(&format!("{e:#}")),
                )
                .await;
                let release = match how {
                    How::Match(release) => release,
                    How::AsIs(_) => None,
                };
                self.deferred.lock().expect("deferred imports").insert(
                    id,
                    (
                        tokio::time::Instant::now() + Duration::from_secs(300),
                        release,
                    ),
                );
            }
            // Another copy of this album is already filed, in this format's
            // folder. What was asked for is in the library, so the job is
            // done and points there; this copy is not kept. Another source
            // would land on the same folder and be refused the same way.
            Err(sift::ImportError::Exists(existing)) => {
                let path = existing.to_string_lossy().to_string();
                db::set_imported(&self.db, id, &path).await?;
                self.event(
                    &job,
                    "imported",
                    None,
                    Some(&format!(
                        "already in the library as {path}; this copy was not kept"
                    )),
                )
                .await;
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
            // As-is refused: the tags are not good enough to file by, which
            // leaves the album where it was, waiting on a decision.
            Err(e @ sift::ImportError::Untagged(_)) => {
                db::set_status(&self.db, id, "review", Some(&format!("{e:#}"))).await?;
                self.event(
                    &job,
                    "review",
                    Some(cause::UNTAGGED),
                    Some(&format!("{e:#}")),
                )
                .await;
            }
            // Files that will not parse are this copy's fault, not the
            // album's: another source is worth trying without being asked.
            Err(e @ (sift::ImportError::Meta(_) | sift::ImportError::Empty(_)))
                if !job.alternates.0.is_empty() =>
            {
                let c = if matches!(e, sift::ImportError::Empty(_)) {
                    cause::NO_AUDIO
                } else {
                    cause::CORRUPT_COPY
                };
                db::set_status(&self.db, id, "failed", Some(&format!("{e:#}"))).await?;
                self.event(&job, "failed", Some(c), Some(&format!("{e:#}")))
                    .await;
                tracing::info!(%id, error = %e, "unreadable copy; trying another source");
                let jobs = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = jobs.next_source(id, c).await {
                        tracing::warn!(%id, error = %e, "could not move to another source");
                    }
                });
            }
            Err(e) => {
                db::set_status(&self.db, id, "failed", Some(&format!("{e:#}"))).await?;
                let c = match e {
                    sift::ImportError::Meta(_) => cause::CORRUPT_COPY,
                    sift::ImportError::Empty(_) => cause::NO_AUDIO,
                    _ => cause::IMPORT_ERROR,
                };
                self.event(&job, "failed", Some(c), Some(&format!("{e:#}")))
                    .await;
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
        // `m4a` holds ALAC or AAC; `analyse` answers None for AAC, which
        // this build does not decode. APE and WavPack have no decoder here
        // at all, so they are not looked at.
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
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match tokio::task::spawn_blocking(move || {
                        crate::analysis::analyse(&path, Some(&png))
                    })
                    .await
                    {
                        Ok(Ok(a)) => a,
                        // A file that does not read is damage, and is not
                        // filed: left out, it would pass the gate unseen.
                        Ok(Err(e)) => Some(crate::analysis::TrackAnalysis::unreadable(
                            name,
                            format!("{e:#}"),
                        )),
                        // A panic is ours, not the file's.
                        Err(e) => {
                            tracing::warn!(file = name, error = %e, "analysis panicked");
                            None
                        }
                    }
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
    /// Take a job waiting on a decision into `importing`, or say why not. A
    /// second request for the same job (a double tap, an assistant beside a
    /// person) is refused here rather than queued behind the first.
    async fn claim(&self, id: Uuid) -> Result<()> {
        if db::claim_import(&self.db, id).await? {
            return Ok(());
        }
        let job = db::job(&self.db, id).await?.context("no such job")?;
        bail!(
            "job is {}; only review, suspect and failed jobs import",
            job.status
        )
    }

    /// Import a suspect job despite its analysis, waiting for the outcome.
    pub async fn approve(self: &Arc<Self>, id: Uuid) -> Result<()> {
        self.claim(id).await?;
        db::set_approved(&self.db, id).await?;
        self.import(id, None).await
    }

    /// File a job by its own tags, for a release MusicBrainz does not have,
    /// waiting for the outcome.
    pub async fn import_as_is(self: &Arc<Self>, id: Uuid, edits: sift::Edits) -> Result<()> {
        self.claim(id).await?;
        self.run_import(id, How::AsIs(edits)).await
    }

    /// Where a job's files are now: filed for a decision once they have
    /// all arrived, in staging while they are still coming.
    async fn files_dir(&self, id: Uuid) -> Result<PathBuf> {
        let job = db::job(&self.db, id).await?.context("no such job")?;
        let done = self.complete_dir(&job);
        Ok(if tokio::fs::try_exists(&done).await.unwrap_or(false) {
            done
        } else {
            self.dir(id)
        })
    }

    /// The job's files and the tags they carry.
    pub async fn tracks(&self, id: Uuid) -> Result<Vec<sift::meta::Track>> {
        Ok(self.tagger.tracks(&self.files_dir(id).await?).await?)
    }

    /// The job's files against one release, track by track.
    /// Whether the tagger consults Discogs, and so can apply a `discogs:`
    /// release.
    pub fn discogs(&self) -> bool {
        self.tagger.cfg.discogs.is_some()
    }

    fn check_release(&self, release: &str) -> Result<()> {
        if release.starts_with("discogs:") && !self.discogs() {
            bail!("Discogs is not enabled; it needs DISCOGS_TOKEN set");
        }
        Ok(())
    }

    pub async fn compare(&self, id: Uuid, release: &str) -> Result<sift::Comparison> {
        self.check_release(release)?;
        Ok(self
            .tagger
            .compare(&self.files_dir(id).await?, release)
            .await?)
    }

    /// `import_as_is` without waiting, for a person tapping a button.
    pub async fn import_as_is_soon(self: &Arc<Self>, id: Uuid) -> Result<()> {
        self.claim(id).await?;
        let jobs = self.clone();
        tokio::spawn(async move {
            if let Err(e) = jobs.run_import(id, How::AsIs(sift::Edits::default())).await {
                tracing::warn!(%id, error = %e, "import as-is failed");
            }
        });
        Ok(())
    }

    /// Import a job as the given release, waiting for the outcome.
    pub async fn resolve(self: &Arc<Self>, id: Uuid, release: String) -> Result<()> {
        self.check_release(&release)?;
        self.claim(id).await?;
        self.import(id, Some(release)).await
    }

    /// `approve` or `resolve` without waiting: the job reads `importing` as
    /// soon as this returns, which is what a person tapping a button sees.
    pub async fn import_soon(
        self: &Arc<Self>,
        id: Uuid,
        approve: bool,
        release: Option<String>,
    ) -> Result<()> {
        if let Some(r) = &release {
            self.check_release(r)?;
        }
        self.claim(id).await?;
        if approve {
            db::set_approved(&self.db, id).await?;
        }
        let jobs = self.clone();
        tokio::spawn(async move {
            if let Err(e) = jobs.import(id, release).await {
                tracing::warn!(%id, error = %e, "import failed");
            }
        });
        Ok(())
    }

    pub fn spectrogram(&self, id: Uuid, n: u32) -> PathBuf {
        self.spectrograms
            .join(id.to_string())
            .join(format!("{n:02}.png"))
    }

    /// Stop a download. Only while downloading: a job further on is filed,
    /// or waiting on a decision, and `remove` is the way to drop it. What
    /// arrived stays, so a retry resumes rather than starts again.
    pub async fn cancel(&self, id: Uuid) -> Result<()> {
        if !db::move_status(&self.db, id, &["downloading"], "cancelled").await? {
            let job = db::job(&self.db, id).await?.context("no such job")?;
            bail!("job is {}; only a downloading job cancels", job.status);
        }
        if let Some(engine) = self.session.engine() {
            for f in db::job_files(&self.db, id).await? {
                engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
            }
        }
        self.forget(id);
        Ok(())
    }

    /// Drop what this process remembers about a job between ticks: a
    /// deferred import in particular would otherwise run after it stopped.
    fn forget(&self, id: Uuid) {
        self.deferred.lock().expect("deferred imports").remove(&id);
        self.retried.lock().expect("retried").remove(&id);
        self.stalled.lock().expect("stalled").remove(&id);
        self.places.lock().expect("places").remove(&id);
        if let Some((task, _)) = self.fetches.lock().expect("fetches").remove(&id) {
            task.abort();
        }
    }

    /// Drop this copy and download the next source found for the request.
    pub async fn next_source(self: &Arc<Self>, id: Uuid, cause: &str) -> Result<()> {
        let engine = self.session.require()?.clone();
        let job = db::job(&self.db, id).await?.context("no such job")?;
        if !matches!(job.status.as_str(), "review" | "suspect" | "failed") {
            bail!(
                "job is {}; only review, suspect and failed jobs change source",
                job.status
            );
        }
        if matches!(job.source.0, Source::Url { .. }) {
            bail!("an album fetched from a URL has no other source; retry it or remove it");
        }
        if job.alternates.0.is_empty() {
            bail!("no other copies were found for this; search again");
        }
        let rows = db::job_files(&self.db, id).await?;
        let _ = tokio::fs::remove_dir_all(self.complete_dir(&job)).await;
        self.fall_back(&engine, &job, &rows, cause).await
    }

    /// Search for the job's title again and move to the best copy found on
    /// another peer, for a job with no fallbacks left. Titles are the query a
    /// grab was given, so this is the grab's own search. False when no other
    /// copy was found and the job was left as it was.
    async fn search_again(
        self: &Arc<Self>,
        engine: &Engine,
        job: &Job,
        cause: &str,
    ) -> Result<bool> {
        let rows = db::job_files(&self.db, job.id).await?;
        let current = rows.first().map(|r| r.peer.clone()).unwrap_or_default();
        let stalled = self.recently_stalled();
        let search = async |filter: &crate::folders::Filter| -> Result<Vec<Folder>> {
            let responses =
                crate::graphql::responses(engine, &job.title, SEARCH_WAIT.as_secs()).await?;
            Ok(
                crate::folders::relevant(crate::folders::group(&responses, filter), &job.title)
                    .into_iter()
                    .filter(|f| f.username != current && !stalled.contains(&f.username))
                    .collect(),
            )
        };
        // The replacement must be as good a copy as the one it replaces:
        // lossless for lossless, and most of the tracks. A copy that is
        // merely online is not a substitute; a lossless album replaced by a
        // one-file video rip is worse than waiting.
        let exts: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                let name = String::from_utf8_lossy(&r.remote).replace('\\', "/");
                std::path::Path::new(&name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
            })
            .filter(|e| crate::folders::AUDIO.contains(&e.as_str()))
            .collect();
        let filter = crate::folders::Filter {
            lossless: !exts.is_empty()
                && exts
                    .iter()
                    .all(|e| crate::folders::LOSSLESS.contains(&e.as_str())),
            min_tracks: Some((exts.len() * 4).div_ceil(5).max(1)),
            ..Default::default()
        };
        let found = search(&filter).await?;
        if found.is_empty() {
            tracing::info!(job = %job.id, "no other copy online; still on {current}");
            return Ok(false);
        }
        tracing::info!(job = %job.id, copies = found.len(), "found other copies; moving off {current}");
        self.stalled_peers
            .lock()
            .expect("stalled peers")
            .insert(current, tokio::time::Instant::now());
        let mut job = job.clone();
        job.alternates = Json(found.into_iter().take(4).map(Alternate::from).collect());
        self.fall_back(engine, &job, &rows, cause).await?;
        Ok(true)
    }

    /// Retry a failed or cancelled job from its current source.
    pub async fn retry(self: &Arc<Self>, id: Uuid) -> Result<()> {
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
            "failed" | "cancelled" => {
                return match job.source.0 {
                    Source::Url { url } => {
                        db::set_status(&self.db, id, "downloading", None).await?;
                        self.fetch(id, url);
                        Ok(())
                    }
                    _ => self.redownload(&self.session.require()?.clone(), id).await,
                };
            }
            s => bail!("job is {s}; only failed, cancelled, review and suspect jobs retry"),
        }
        self.import_soon(id, false, None).await
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
        // Its files may be on their way into the library, and deleting the
        // folder then would leave the album half there. One statement takes
        // it out of every state an import can start from, so none starts
        // between the check and the delete.
        const NOT_IMPORTING: &[&str] = &[
            "downloading",
            "review",
            "suspect",
            "failed",
            "cancelled",
            "imported",
        ];
        if !db::move_status(&self.db, id, NOT_IMPORTING, "removing").await? {
            match db::job(&self.db, id).await? {
                Some(j) if j.status == "importing" => {
                    bail!("the album is being filed into the library; remove it once that finishes")
                }
                Some(_) => {}
                None => return Ok(()),
            }
        }
        self.forget(id);
        if let Some(engine) = self.session.engine() {
            for f in db::job_files(&self.db, id).await? {
                engine.remove_download(&f.peer, &RawStr(f.remote.clone().into()));
            }
        }
        let _ = tokio::fs::remove_dir_all(self.dir(id)).await;
        let _ = tokio::fs::remove_file(self.fetch_file(id)).await;
        let _ = tokio::fs::remove_dir_all(self.spectrograms.join(id.to_string())).await;
        if let Some(job) = db::job(&self.db, id).await? {
            let _ = tokio::fs::remove_dir_all(self.complete_dir(&job)).await;
        }
        // A wish that found this album would otherwise find it again on its
        // next pass: removing the album is the answer to the wish too.
        self.db
            .run(move |c| c.execute("DELETE FROM wishes WHERE job_id = ?1", [id]))
            .await?;
        db::delete_job(&self.db, id).await?;
        Ok(())
    }
}

/// A URL a job may fetch: http or https, with a host.
pub(crate) fn check_url(s: &str) -> Result<url::Url> {
    let url = url::Url::parse(s.trim()).context("not a URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "only http and https URLs can be fetched"
    );
    anyhow::ensure!(
        url.host_str().is_some_and(|h| !h.is_empty()),
        "the URL names no host"
    );
    Ok(url)
}

/// A URL's host, which is all of it that is logged or shown: download links
/// carry access tokens.
pub(crate) fn host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// The name a fetched file is saved under: the one the server gives in
/// `Content-Disposition`, else the URL's last path segment, reduced to a
/// plain file name.
fn file_name(disposition: Option<&str>, url: &url::Url) -> Option<String> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    let params: Vec<(String, &str)> = disposition
        .unwrap_or_default()
        .split(';')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
        .collect();
    let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let named = param("filename*")
        .and_then(|v| v.split_once("''"))
        .map(|(_, v)| decode(v))
        .or_else(|| param("filename").map(|v| v.trim_matches('"').to_string()))
        .or_else(|| url.path_segments()?.next_back().map(decode))?;
    let name = named.rsplit(['/', '\\']).next().unwrap_or_default();
    let name = name.replace('\0', "");
    let name = name.trim().trim_start_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

/// Unpack the zip at `archive` into `dir`. An entry that would land outside
/// `dir` refuses the archive, as does unpacking more than `limit` bytes. A
/// single folder holding everything is unwrapped, so the files sit at the
/// top as a download's do.
pub(crate) fn unzip(archive: &std::path::Path, dir: &std::path::Path, limit: u64) -> Result<()> {
    use std::path::Component;
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?).context("not a zip")?;
    let mut entries = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        // `enclosed_name` strips a leading root from a name rather than
        // refusing it; a name it had to change is refused here.
        let Some(path) = entry.enclosed_name().filter(|p| {
            p.as_path() == std::path::Path::new(entry.name().as_deref().unwrap_or_default())
        }) else {
            bail!("the zip holds a path outside itself: {}", entry.name()?);
        };
        // Folders are made as needed; links and Finder's resource forks
        // are not the album.
        if entry.is_dir()
            || entry.is_symlink()
            || path.components().next() == Some(Component::Normal("__MACOSX".as_ref()))
        {
            continue;
        }
        entries.push((i, path));
    }
    let top = entries.first().and_then(|(_, p)| p.components().next());
    let wrapped = top.is_some()
        && entries
            .iter()
            .all(|(_, p)| p.components().count() > 1 && p.components().next() == top);
    let mut left = limit;
    for (i, path) in &entries {
        let rel: std::path::PathBuf = path.components().skip(usize::from(wrapped)).collect();
        let out = dir.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = zip.by_index(*i)?;
        let mut file = std::fs::File::create(&out)?;
        let n = std::io::copy(&mut std::io::Read::take(entry, left + 1), &mut file)
            .with_context(|| format!("unpacking {}", rel.display()))?;
        left = left
            .checked_sub(n)
            .with_context(|| format!("the zip unpacks to more than {limit} bytes"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::*;

    fn zip_of(files: &[(&str, &[u8])]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("slsk-zip-{}.zip", Uuid::new_v4()));
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    /// Unpacked into a directory of its own, inside another of its own so
    /// that an escape is seen.
    fn unpacked(files: &[(&str, &[u8])], limit: u64) -> (Result<()>, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("slsk-unzip-{}", Uuid::new_v4()))
            .join("job");
        std::fs::create_dir_all(&dir).unwrap();
        (unzip(&zip_of(files), &dir, limit), dir)
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_zip_unpacks_where_it_is_put() {
        let (r, dir) = unpacked(
            &[
                ("01 One.flac", b"one"),
                ("cover.jpg", b"img"),
                ("CD2/01 Two.flac", b"two"),
            ],
            1 << 20,
        );
        r.unwrap();
        assert_eq!(
            listing(&dir),
            ["01 One.flac", "CD2/01 Two.flac", "cover.jpg"]
        );
        assert_eq!(std::fs::read(dir.join("01 One.flac")).unwrap(), b"one");
    }

    #[test]
    fn one_folder_holding_everything_is_unwrapped() {
        let (r, dir) = unpacked(
            &[
                ("Burial - Untrue/01 Untitled.flac", b"a"),
                ("Burial - Untrue/CD2/01 Archangel.flac", b"b"),
                ("__MACOSX/Burial - Untrue/._01 Untitled.flac", b"fork"),
            ],
            1 << 20,
        );
        r.unwrap();
        assert_eq!(listing(&dir), ["01 Untitled.flac", "CD2/01 Archangel.flac"]);
    }

    #[test]
    fn a_folder_beside_loose_files_is_kept() {
        let (r, dir) = unpacked(&[("A/01.flac", b"a"), ("cover.jpg", b"c")], 1 << 20);
        r.unwrap();
        assert_eq!(listing(&dir), ["A/01.flac", "cover.jpg"]);
    }

    #[test]
    fn a_path_out_of_the_zip_refuses_it() {
        for name in ["../escape.flac", "/etc/escape.flac", "a/../../escape.flac"] {
            let (r, dir) = unpacked(&[("01.flac", b"a"), (name, b"x")], 1 << 20);
            assert!(r.is_err(), "{name} was unpacked");
            assert!(!dir.parent().unwrap().join("escape.flac").exists());
            assert!(listing(dir.parent().unwrap()).is_empty());
        }
    }

    #[test]
    fn a_zip_bigger_than_the_limit_is_refused() {
        let (r, _) = unpacked(&[("01.flac", &[0; 600]), ("02.flac", &[0; 600])], 1000);
        assert!(r.is_err());
        let (r, _) = unpacked(&[("01.flac", &[0; 600]), ("02.flac", &[0; 400])], 1000);
        r.unwrap();
    }

    #[test]
    fn only_web_urls_are_fetched() {
        assert!(check_url(" https://p4.bcbits.com/download/album/x?token=y ").is_ok());
        assert!(check_url("http://example.com/a.flac").is_ok());
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/a.zip",
            "example.com/a.zip",
            "",
            "https://",
        ] {
            assert!(check_url(bad).is_err(), "{bad} was accepted");
        }
        assert_eq!(
            host("https://p4.bcbits.com/download?token=secret"),
            "p4.bcbits.com"
        );
    }

    #[test]
    fn a_fetched_file_is_named_by_the_server_then_the_url() {
        let url =
            url::Url::parse("https://p4.bcbits.com/download/album/abc%20def.zip?token=t").unwrap();
        assert_eq!(
            file_name(
                Some(
                    "attachment; filename*=UTF-8''Burial%20-%20Untrue.zip; filename=\"Burial - Untrue.zip\""
                ),
                &url
            )
            .as_deref(),
            Some("Burial - Untrue.zip")
        );
        assert_eq!(
            file_name(
                Some("attachment; filename=\"Kode9 - Escapology.zip\""),
                &url
            )
            .as_deref(),
            Some("Kode9 - Escapology.zip")
        );
        assert_eq!(file_name(None, &url).as_deref(), Some("abc def.zip"));
        assert_eq!(
            file_name(Some("attachment; filename=\"../../.ssh/keys\""), &url).as_deref(),
            Some("keys")
        );
        let bare = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(file_name(None, &bare), None);
    }
}
