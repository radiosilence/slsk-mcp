//! The GraphQL schema: what the MCP tool, the web UI and the gateway's
//! credential check all speak.

use std::sync::Arc;
use std::time::Duration;

use async_graphql::{
    Context, EmptySubscription, Enum, Error, ID, Object, Result, Schema, SimpleObject,
};
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::{Status, TransferView};
use uuid::Uuid;

use crate::App;
use crate::db::{self, Alternate, Source};
use crate::folders::{self, Filter, Folder};

pub type SlskSchema = Schema<Query, Mutation, EmptySubscription>;

pub fn schema(app: Arc<App>) -> SlskSchema {
    Schema::build(Query, Mutation, EmptySubscription)
        .data(app)
        .limit_depth(8)
        .finish()
}

pub fn sdl() -> String {
    Schema::build(Query, Mutation, EmptySubscription)
        .finish()
        .sdl()
}

fn app<'a>(ctx: &Context<'a>) -> &'a Arc<App> {
    ctx.data_unchecked::<Arc<App>>()
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
enum SessionState {
    /// No account has been configured.
    NoAccount,
    Connecting,
    Connected,
    /// The server refused the credentials.
    InvalidCredentials,
    /// Another client logged in with this account.
    Displaced,
    Disconnected,
}

#[derive(SimpleObject)]
struct SessionStatus {
    state: SessionState,
    username: Option<String>,
    detail: Option<String>,
    shared_files: usize,
    shared_folders: usize,
    /// Our parent in the distributed search network, if adopted.
    distributed_parent: Option<String>,
    distributed_children: usize,
}

#[derive(SimpleObject)]
pub(crate) struct Transfer {
    pub id: ID,
    pub username: String,
    pub filename: String,
    pub size: u64,
    pub bytes: u64,
    /// Bytes per second.
    pub speed: u64,
    pub state: String,
    /// Position in the uploader's queue, when they have said.
    pub place: Option<u32>,
    pub error: Option<String>,
}

impl From<TransferView> for Transfer {
    fn from(t: TransferView) -> Self {
        Self {
            id: ID(t.id.to_string()),
            username: t.username,
            filename: t.filename.to_string_lossy(),
            size: t.size,
            bytes: t.bytes,
            speed: t.speed,
            state: t.state.into(),
            place: t.place,
            error: t.error,
        }
    }
}

#[derive(SimpleObject)]
pub(crate) struct JobFile {
    pub name: String,
    pub size: u64,
    pub bytes: u64,
    pub state: String,
    pub error: Option<String>,
}

#[derive(SimpleObject)]
pub(crate) struct TriageCause {
    /// peer_failed, stalled_peer, corrupt_copy, no_audio, requested, no_candidates,
    /// incomplete, extra_files, weak_match, lossy_source, upsampled,
    /// mb_unavailable or import_error.
    pub cause: String,
    pub count: usize,
    pub examples: Vec<TriageEvent>,
}

#[derive(SimpleObject)]
pub(crate) struct TriageEvent {
    /// The job, if it still exists; its history outlives it.
    pub job_id: ID,
    pub title: String,
    pub at: chrono::DateTime<chrono::Utc>,
    /// The release that produced this outcome.
    pub version: String,
    pub outcome: String,
    pub detail: Option<String>,
}

#[derive(SimpleObject)]
pub(crate) struct Job {
    pub id: ID,
    pub title: String,
    /// downloading, importing, imported, review (the tagger could not choose a
    /// release), suspect (the audio looks transcoded), failed or cancelled.
    pub status: String,
    pub error: Option<String>,
    pub username: Option<String>,
    pub folder: Option<String>,
    /// Other sources that matched the same request, tried if this one fails;
    /// `nextSource` moves to the next one on request.
    pub alternates: usize,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub files: Vec<JobFile>,
    /// For a job in review: releases to choose from. Resolve with
    /// `resolveJob(id, releaseId)`.
    pub candidates: Vec<Candidate>,
    pub library_path: Option<String>,
    /// Per-track spectral analysis, taken before import. A job held as
    /// `suspect` failed it; `approveJob` imports it anyway.
    pub analysis: Vec<crate::analysis::TrackAnalysis>,
    pub import_log: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) async fn job_view(app: &App, job: db::Job, with_files: bool) -> Result<Job> {
    let rows = db::job_files(&app.db, job.id).await?;
    let engine = app.session.engine();
    let mut files = Vec::with_capacity(rows.len());
    let (mut total, mut done) = (0u64, 0u64);
    for f in rows {
        let remote = RawStr(f.remote.clone().into());
        let live = engine.and_then(|e| e.download_view(&f.peer, &remote));
        let bytes = match (&live, f.state.as_str()) {
            (Some(v), _) => v.bytes,
            (None, "completed") => f.size as u64,
            _ => 0,
        };
        total += f.size as u64;
        done += bytes;
        if with_files {
            let name = remote.to_string_lossy();
            files.push(JobFile {
                name: name.rsplit('\\').next().unwrap_or(&name).to_string(),
                size: f.size as u64,
                bytes,
                state: live
                    .as_ref()
                    .map_or(f.state.clone(), |v| v.state.to_string()),
                error: live.and_then(|v| v.error).or(f.error),
            });
        }
    }
    let (username, folder) = match &job.source.0 {
        Source::Soulseek {
            username, folder, ..
        } => (Some(username.clone()), Some(folder.clone())),
        Source::Files { username } => (Some(username.clone()), None),
    };
    Ok(Job {
        id: ID(job.id.to_string()),
        title: job.title,
        status: job.status,
        error: job.error,
        username,
        folder,
        alternates: job.alternates.0.len(),
        total_bytes: total,
        downloaded_bytes: done,
        files,
        candidates: job
            .candidates
            .map(|c| c.0.into_iter().map(Candidate::from).collect())
            .unwrap_or_default(),
        library_path: job.library_path,
        analysis: job.analysis.map(|a| a.0).unwrap_or_default(),
        import_log: job.import_log,
        created_at: job.created_at,
        updated_at: job.updated_at,
    })
}

/// A MusicBrainz release the tagger considered.
#[derive(SimpleObject)]
pub(crate) struct Candidate {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
    pub media: Option<String>,
    pub disambiguation: Option<String>,
    pub tracks: usize,
    /// 0 is a perfect match; the tagger applies matches below 0.04 itself.
    pub distance: f64,
    /// Release tracks with no file.
    pub missing: usize,
    /// Files with no release track.
    pub extra: usize,
}

impl From<sift::Candidate> for Candidate {
    fn from(c: sift::Candidate) -> Self {
        Self {
            release_id: c.id,
            title: c.title,
            artist: c.artist,
            date: c.date,
            country: c.country,
            media: c.media,
            disambiguation: c.disambiguation,
            tracks: c.tracks,
            distance: c.distance,
            missing: c.missing,
            extra: c.extra,
        }
    }
}

#[derive(SimpleObject)]
struct Directory {
    path: String,
    files: Vec<folders::File>,
}

#[derive(SimpleObject)]
struct PeerInfo {
    username: String,
    description: String,
    total_uploads: u32,
    queue_size: u32,
    slots_free: bool,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
enum SendAction {
    /// Returns a token and sends nothing.
    Preview,
    /// Sends, given the token PREVIEW returned for exactly this message.
    Confirm,
}

#[derive(SimpleObject)]
struct Sent {
    /// For PREVIEW: pass to the CONFIRM call.
    confirmation_token: Option<String>,
    /// What would be, or was, sent, as the recipient will see it.
    preview: String,
    sent: bool,
}

#[derive(SimpleObject)]
struct Room {
    name: String,
    members: Vec<String>,
    messages: Vec<crate::social::RoomLine>,
}

#[derive(SimpleObject)]
struct PublicRoom {
    name: String,
    users: u32,
}

#[derive(SimpleObject)]
struct Conversation {
    username: String,
    last_message: String,
    last_was_ours: bool,
    at: chrono::DateTime<chrono::Utc>,
    unread: i64,
}

#[derive(SimpleObject)]
struct Message {
    body: String,
    outgoing: bool,
    at: chrono::DateTime<chrono::Utc>,
}

#[derive(SimpleObject)]
struct Buddy {
    username: String,
    note: String,
    /// online, away, offline; unknown until the server has said.
    status: Option<String>,
    privileged: bool,
}

pub struct Query;

#[Object]
impl Query {
    /// Whether we are logged in, as whom, and what we share.
    async fn status(&self, ctx: &Context<'_>) -> SessionStatus {
        let Some(engine) = app(ctx).session.engine() else {
            return SessionStatus {
                state: SessionState::NoAccount,
                username: None,
                detail: None,
                shared_files: 0,
                shared_folders: 0,
                distributed_parent: None,
                distributed_children: 0,
            };
        };
        let (state, detail) = match &*engine.status().borrow() {
            Status::Connecting => (SessionState::Connecting, None),
            Status::LoggedIn { .. } => (SessionState::Connected, None),
            Status::Rejected { reason } => (SessionState::InvalidCredentials, Some(reason.clone())),
            Status::Displaced => (
                SessionState::Displaced,
                Some("another client logged in with this account".into()),
            ),
            Status::Disconnected { error } => (SessionState::Disconnected, Some(error.clone())),
        };
        let (folders, files) = engine.share_counts();
        let (parent, _, _, children) = engine.distributed();
        SessionStatus {
            state,
            username: Some(engine.username()),
            detail,
            shared_files: files,
            shared_folders: folders,
            distributed_parent: parent,
            distributed_children: children,
        }
    }

    /// Search the network and group the results into folders, best first.
    /// Waits `waitSeconds` (default 8, at most 30) for peers to answer; there
    /// is no end-of-results signal, and most answers land within ten seconds.
    async fn search(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(default = 8)] wait_seconds: u64,
        #[graphql(default)] filter: Filter,
        #[graphql(default = 20)] limit: usize,
    ) -> Result<Vec<Folder>> {
        let folders = search(app(ctx), &query, wait_seconds, &filter).await?;
        Ok(folders.into_iter().take(limit.min(200)).collect())
    }

    async fn downloads(
        &self,
        ctx: &Context<'_>,
        #[graphql(default)] active_only: bool,
    ) -> Vec<Transfer> {
        let Some(e) = app(ctx).session.engine() else {
            return Vec::new();
        };
        e.downloads()
            .into_iter()
            .filter(|t| !active_only || !matches!(t.state, "completed" | "failed" | "cancelled"))
            .map(Transfer::from)
            .collect()
    }

    async fn uploads(&self, ctx: &Context<'_>) -> Vec<Transfer> {
        app(ctx)
            .session
            .engine()
            .map(|e| e.uploads().into_iter().map(Transfer::from).collect())
            .unwrap_or_default()
    }

    /// Albums on their way into the library, newest first.
    async fn jobs(
        &self,
        ctx: &Context<'_>,
        status: Option<String>,
        #[graphql(default = 50)] first: i64,
    ) -> Result<Vec<Job>> {
        let app = app(ctx);
        let mut out = Vec::new();
        for j in db::jobs(&app.db, status.as_deref(), first.clamp(1, 500)).await? {
            out.push(job_view(app, j, false).await?);
        }
        Ok(out)
    }

    async fn job(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Job>> {
        let app = app(ctx);
        match db::job(&app.db, parse_id(&id)?).await? {
            Some(j) => Ok(Some(job_view(app, j, true).await?)),
            None => Ok(None),
        }
    }

    /// A user's shared folders. `path` narrows to folders under it; large
    /// collections run to tens of thousands of folders.
    async fn browse(
        &self,
        ctx: &Context<'_>,
        username: String,
        path: Option<String>,
        #[graphql(default = 200)] limit: usize,
    ) -> Result<Vec<Directory>> {
        let engine = app(ctx).session.require()?;
        let dirs = engine.browse(&username).await?;
        let prefix = path.map(|p| p.to_lowercase());
        Ok(dirs
            .into_iter()
            .map(|d| (d.name.to_string_lossy(), d))
            .filter(|(name, _)| {
                prefix
                    .as_ref()
                    .is_none_or(|p| name.to_lowercase().starts_with(p))
            })
            .take(limit.min(2000))
            .map(|(path, d)| Directory {
                files: d
                    .files
                    .iter()
                    .map(|f| {
                        let mut full = d.name.as_bytes().to_vec();
                        full.push(b'\\');
                        full.extend_from_slice(f.name.as_bytes());
                        let remote = RawStr(full.into());
                        folders::File {
                            name: f.name.to_string_lossy(),
                            remote,
                            size: f.size,
                            extension: f.extension.clone(),
                            bitrate: f.attr(0),
                            duration: f.attr(1),
                            sample_rate: f.attr(4),
                            bit_depth: f.attr(5),
                        }
                    })
                    .collect(),
                path,
            })
            .collect())
    }

    async fn user_info(&self, ctx: &Context<'_>, username: String) -> Result<PeerInfo> {
        let info = app(ctx).session.require()?.user_info(&username).await?;
        Ok(PeerInfo {
            username,
            description: info.description,
            total_uploads: info.total_uploads,
            queue_size: info.queue_size,
            slots_free: info.slots_free,
        })
    }

    /// Rooms we are in, with members and the most recent lines.
    async fn rooms(&self, ctx: &Context<'_>, #[graphql(default = 50)] tail: usize) -> Vec<Room> {
        app(ctx)
            .social
            .rooms(tail.min(500))
            .into_iter()
            .map(|(name, members, messages)| Room {
                name,
                members,
                messages,
            })
            .collect()
    }

    /// Public rooms and how many are in each, busiest first.
    async fn room_list(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: usize,
    ) -> Vec<PublicRoom> {
        app(ctx)
            .social
            .room_list()
            .into_iter()
            .take(first)
            .map(|(name, users)| PublicRoom { name, users })
            .collect()
    }

    /// Private conversations, one row per user.
    async fn conversations(&self, ctx: &Context<'_>) -> Result<Vec<Conversation>> {
        Ok(app(ctx)
            .social
            .conversations()
            .await?
            .into_iter()
            .map(
                |(username, last_message, last_was_ours, at, unread)| Conversation {
                    username,
                    last_message,
                    last_was_ours,
                    at,
                    unread,
                },
            )
            .collect())
    }

    async fn messages(
        &self,
        ctx: &Context<'_>,
        username: String,
        #[graphql(default = 50)] last: i64,
    ) -> Result<Vec<Message>> {
        Ok(app(ctx)
            .social
            .messages(&username, last.clamp(1, 1000))
            .await?
            .into_iter()
            .map(|(body, outgoing, at)| Message { body, outgoing, at })
            .collect())
    }

    async fn buddies(&self, ctx: &Context<'_>) -> Result<Vec<Buddy>> {
        Ok(app(ctx)
            .social
            .buddies()
            .await?
            .into_iter()
            .map(|(username, note, status, privileged)| Buddy {
                username,
                note,
                status: status.map(|s| match s {
                    slsk_engine::slsk_proto::server::UserStatus::Online => "online".into(),
                    slsk_engine::slsk_proto::server::UserStatus::Away => "away".into(),
                    slsk_engine::slsk_proto::server::UserStatus::Offline => "offline".into(),
                }),
                privileged,
            })
            .collect())
    }

    /// Searches repeated on the server's wishlist interval until found.
    async fn wishlist(&self, ctx: &Context<'_>) -> Result<Vec<crate::social::Wish>> {
        Ok(app(ctx).social.wishes().await?)
    }

    /// Why albums did not land cleanly, grouped by cause, most frequent
    /// first, each with its most recent examples. A cause that keeps
    /// recurring is a fix to make in code; `version` says which release
    /// produced each, so jobs mishandled before a fix can be found and
    /// repaired after it.
    async fn triage(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 7)] days: i64,
        #[graphql(default = 5)] examples: usize,
    ) -> Result<Vec<TriageCause>> {
        let since = chrono::Utc::now() - chrono::Duration::days(days.clamp(1, 365));
        let events = db::events_since(&app(ctx).db, since).await?;
        let mut by_cause: std::collections::BTreeMap<String, TriageCause> = Default::default();
        for e in events {
            let Some(cause) = e.cause.clone() else {
                continue;
            };
            let entry = by_cause
                .entry(cause.clone())
                .or_insert_with(|| TriageCause {
                    cause,
                    count: 0,
                    examples: Vec::new(),
                });
            entry.count += 1;
            if entry.examples.len() < examples {
                entry.examples.push(TriageEvent {
                    job_id: ID(e.job_id.to_string()),
                    title: e.title,
                    at: e.at,
                    version: e.version,
                    outcome: e.outcome,
                    detail: e.detail,
                });
            }
        }
        let mut causes: Vec<TriageCause> = by_cause.into_values().collect();
        causes.sort_by_key(|c| std::cmp::Reverse(c.count));
        Ok(causes)
    }

    async fn bans(&self, ctx: &Context<'_>) -> Result<Vec<String>> {
        let app = app(ctx);
        let engine = app.session.require()?;
        Ok(db::bans(&app.db, &engine.username()).await?)
    }
}

pub(crate) async fn search(
    app: &App,
    query: &str,
    wait: u64,
    filter: &Filter,
) -> Result<Vec<Folder>> {
    let engine = app.session.require()?;
    let mut rx = engine.search(query)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait.clamp(1, 30));
    let mut responses = Vec::new();
    while let Ok(Some(r)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        responses.push(r);
    }
    Ok(folders::group(&responses, filter))
}

/// Search, pick the best relevant folder, keep four fallbacks, start a job.
/// Without a filter it prefers lossless and settles for lossy only when
/// there is no lossless copy at all.
/// Peers holding downloads of ours in their queue while sending none.
fn stuck_peers(downloads: &[slsk_engine::TransferView]) -> std::collections::HashSet<String> {
    let mut queued = std::collections::HashSet::new();
    let mut sending = std::collections::HashSet::new();
    for t in downloads {
        match t.state {
            "remote_queued" => {
                queued.insert(t.username.clone());
            }
            "starting" | "transferring" => {
                sending.insert(t.username.clone());
            }
            _ => {}
        }
    }
    queued.retain(|u| !sending.contains(u));
    queued
}

pub(crate) async fn grab(
    app: &App,
    query: &str,
    wait: u64,
    filter: Option<Filter>,
) -> Result<db::Job> {
    let strict = filter.is_some();
    let filter = filter.unwrap_or(Filter {
        lossless: true,
        ..Default::default()
    });
    let mut found = folders::relevant(search(app, query, wait, &filter).await?, query);
    if found.is_empty() && !strict {
        found = folders::relevant(search(app, query, wait, &Filter::default()).await?, query);
    }
    // A peer we are already queued with and receiving nothing from answers
    // searches readily and sends nothing; its folders go last.
    if let Some(engine) = app.session.engine() {
        let stuck = stuck_peers(&engine.downloads());
        found.sort_by_key(|f| stuck.contains(&f.username));
    }
    let mut found = found.into_iter();
    let best = found
        .next()
        .ok_or_else(|| Error::new(format!("nothing found for {query:?}")))?;
    let alternates = found
        .take(4)
        .map(|f| Alternate {
            username: f.username,
            folder: f.path,
            folder_raw: f.remote_path.as_bytes().to_vec(),
        })
        .collect();
    Ok(app
        .jobs
        .from_folder(&best, Some(query.to_string()), alternates)
        .await?)
}

fn parse_id(id: &ID) -> Result<Uuid> {
    Uuid::parse_str(id).map_err(|_| Error::new("not a job id"))
}

pub struct Mutation;

#[Object]
impl Mutation {
    /// Find an album and start fetching the best copy of it. The best copy is
    /// lossless (unless `filter.lossless` is false and nothing lossless
    /// exists), from a peer with a free slot, and mentions every word of the
    /// query in its path. The next four candidates are kept as fallbacks.
    /// Poll `job(id)` for progress; it ends `imported`, `review` or `failed`.
    async fn grab(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(default = 10)] wait_seconds: u64,
        filter: Option<Filter>,
    ) -> Result<Job> {
        let app = app(ctx);
        let job = grab(app, &query, wait_seconds, filter).await?;
        job_view(app, job, true).await
    }

    /// Fetch one folder from one user, as found by `search` or `browse`.
    async fn download_folder(
        &self,
        ctx: &Context<'_>,
        username: String,
        folder: String,
        title: Option<String>,
    ) -> Result<Job> {
        let app = app(ctx);
        let remote = RawStr::from(folder.clone());
        let f = Folder {
            username,
            path: folder,
            remote_path: remote,
            files: Vec::new(),
            audio_files: 0,
            total_size: 0,
            lossless: false,
            bit_depth: None,
            sample_rate: None,
            bitrate: None,
            free_slot: false,
            speed: 0,
            queue_length: 0,
            score: 0.0,
        };
        let job = app.jobs.from_folder(&f, title, Vec::new()).await?;
        job_view(app, job, true).await
    }

    /// Fetch individual files (full paths as `search` or `browse` gave them)
    /// from one user.
    async fn download_files(
        &self,
        ctx: &Context<'_>,
        username: String,
        files: Vec<String>,
        sizes: Vec<u64>,
        title: String,
    ) -> Result<Job> {
        if files.len() != sizes.len() {
            return Err(Error::new("files and sizes must be the same length"));
        }
        let app = app(ctx);
        let list = files.into_iter().map(RawStr::from).zip(sizes).collect();
        let job = app.jobs.from_files(&username, list, title).await?;
        job_view(app, job, true).await
    }

    /// Import a job in review as the given MusicBrainz release.
    async fn resolve_job(&self, ctx: &Context<'_>, id: ID, release_id: String) -> Result<Job> {
        let app = app(ctx);
        let id = parse_id(&id)?;
        app.jobs.resolve(id, release_id).await?;
        job_view(
            app,
            db::job(&app.db, id)
                .await?
                .ok_or_else(|| Error::new("no such job"))?,
            true,
        )
        .await
    }

    /// Import a job held as `suspect` despite its analysis.
    async fn approve_job(&self, ctx: &Context<'_>, id: ID) -> Result<Job> {
        let app = app(ctx);
        let id = parse_id(&id)?;
        app.jobs.approve(id).await?;
        job_view(
            app,
            db::job(&app.db, id)
                .await?
                .ok_or_else(|| Error::new("no such job"))?,
            true,
        )
        .await
    }

    async fn retry_job(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        app(ctx).jobs.retry(parse_id(&id)?).await?;
        Ok(true)
    }

    /// Drop the copy a job holds (in review, suspect or failed) and download
    /// the next folder found for the same request: for a transcode, or a rip
    /// no release fits.
    async fn next_source(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        app(ctx)
            .jobs
            .next_source(parse_id(&id)?, crate::jobs::cause::REQUESTED)
            .await?;
        Ok(true)
    }

    async fn cancel_job(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        app(ctx).jobs.cancel(parse_id(&id)?).await?;
        Ok(true)
    }

    /// Forget a job and delete whatever it downloaded that was not imported.
    async fn remove_job(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        app(ctx).jobs.remove(parse_id(&id)?).await?;
        Ok(true)
    }

    async fn cancel_upload(
        &self,
        ctx: &Context<'_>,
        username: String,
        filename: String,
    ) -> Result<bool> {
        Ok(app(ctx)
            .session
            .require()?
            .cancel_upload(&username, &RawStr::from(filename)))
    }

    /// Refuse uploads, browsing and search results to a user, and drop what
    /// they have queued.
    async fn ban(&self, ctx: &Context<'_>, username: String) -> Result<bool> {
        set_ban(app(ctx), &username, true).await
    }

    async fn unban(&self, ctx: &Context<'_>, username: String) -> Result<bool> {
        set_ban(app(ctx), &username, false).await
    }

    async fn rescan_shares(&self, ctx: &Context<'_>) -> Result<bool> {
        let engine = app(ctx).session.require()?.clone();
        tokio::spawn(async move { engine.rescan().await });
        Ok(true)
    }

    async fn set_upload_slots(&self, ctx: &Context<'_>, slots: usize) -> Result<bool> {
        app(ctx)
            .session
            .require()?
            .set_upload_slots(slots.clamp(1, 100));
        Ok(true)
    }

    /// Bytes per second; 0 is unlimited.
    async fn set_speed_limits(
        &self,
        ctx: &Context<'_>,
        upload: u64,
        download: u64,
    ) -> Result<bool> {
        app(ctx).session.require()?.set_limits(upload, download);
        Ok(true)
    }

    /// Send a private message. Messages go to a person: call with PREVIEW,
    /// show the user the preview, and only then call with CONFIRM and the
    /// token, unchanged.
    async fn send_message(
        &self,
        ctx: &Context<'_>,
        username: String,
        message: String,
        action: SendAction,
        confirmation_token: Option<String>,
    ) -> Result<Sent> {
        let social = &app(ctx).social;
        match action {
            SendAction::Preview => Ok(Sent {
                confirmation_token: Some(social.preview("pm", &username, &message)),
                preview: format!("To {username}: {message}"),
                sent: false,
            }),
            SendAction::Confirm => {
                let token = confirmation_token
                    .ok_or_else(|| Error::new("CONFIRM needs the token PREVIEW returned"))?;
                social.send_message(&token, &username, &message).await?;
                Ok(Sent {
                    confirmation_token: None,
                    preview: format!("To {username}: {message}"),
                    sent: true,
                })
            }
        }
    }

    /// Say something in a room, with the same PREVIEW/CONFIRM steps.
    async fn say(
        &self,
        ctx: &Context<'_>,
        room: String,
        message: String,
        action: SendAction,
        confirmation_token: Option<String>,
    ) -> Result<Sent> {
        let social = &app(ctx).social;
        match action {
            SendAction::Preview => Ok(Sent {
                confirmation_token: Some(social.preview("room", &room, &message)),
                preview: format!("In {room}: {message}"),
                sent: false,
            }),
            SendAction::Confirm => {
                let token = confirmation_token
                    .ok_or_else(|| Error::new("CONFIRM needs the token PREVIEW returned"))?;
                social.say(&token, &room, &message)?;
                Ok(Sent {
                    confirmation_token: None,
                    preview: format!("In {room}: {message}"),
                    sent: true,
                })
            }
        }
    }

    async fn mark_read(&self, ctx: &Context<'_>, username: String) -> Result<bool> {
        app(ctx).social.mark_read(&username).await?;
        Ok(true)
    }

    /// Join a room; it is rejoined after every reconnect until left.
    async fn join_room(&self, ctx: &Context<'_>, room: String) -> Result<bool> {
        app(ctx).social.join_room(&room).await?;
        Ok(true)
    }

    async fn leave_room(&self, ctx: &Context<'_>, room: String) -> Result<bool> {
        app(ctx).social.leave_room(&room).await?;
        Ok(true)
    }

    /// Watch a user: their status is followed while we are online.
    async fn add_buddy(
        &self,
        ctx: &Context<'_>,
        username: String,
        #[graphql(default)] note: String,
    ) -> Result<bool> {
        app(ctx).social.add_buddy(&username, &note).await?;
        Ok(true)
    }

    async fn remove_buddy(&self, ctx: &Context<'_>, username: String) -> Result<bool> {
        app(ctx).social.remove_buddy(&username).await?;
        Ok(true)
    }

    /// Keep searching for something nobody has shared yet. With `grab`, the
    /// first relevant folder found becomes a job, as `grab` would.
    async fn add_wish(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(default = true)] lossless: bool,
        #[graphql(default)] grab: bool,
    ) -> Result<ID> {
        Ok(ID(app(ctx)
            .social
            .add_wish(&query, lossless, grab)
            .await?
            .to_string()))
    }

    async fn remove_wish(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        app(ctx).social.remove_wish(parse_id(&id)?).await?;
        Ok(true)
    }

    /// Like (true), dislike (false) or forget (null) an interest. Interests
    /// drive the server's recommendations and similar users.
    async fn set_interest(
        &self,
        ctx: &Context<'_>,
        item: String,
        liked: Option<bool>,
    ) -> Result<bool> {
        app(ctx).social.set_interest(&item, liked).await?;
        Ok(true)
    }

    /// Log in again after being displaced by another client.
    async fn reconnect(&self, ctx: &Context<'_>) -> Result<bool> {
        app(ctx).session.require()?.reconnect();
        Ok(true)
    }
}

async fn set_ban(app: &App, username: &str, banned: bool) -> Result<bool> {
    let engine = app.session.require()?;
    let account = engine.username();
    db::set_ban(&app.db, &account, username, banned).await?;
    engine.set_banned(db::bans(&app.db, &account).await?);
    Ok(true)
}
