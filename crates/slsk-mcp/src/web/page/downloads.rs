//! The Downloads tab: the jobs in hand, card by card, then the ones done
//! with; each job's own page; and what can be done to a job. An action
//! changes the job and answers with nothing more: the page's stream draws
//! the change, which is the confirmation.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use koan_kit_web as kit;
use uuid::Uuid;

use super::{Body, Page, Tab, View};
use crate::App;
use crate::graphql;
use crate::web::{Draw, MOST, UiState, failed, fields, human, place_url};

pub(in crate::web) mod card;
pub(in crate::web) mod job;

use card::{Card, Lazy};

pub(in crate::web) struct Downloads;

/// The Downloads address once drew searches, at `/?q=…`.
#[derive(serde::Deserialize, Default)]
pub(in crate::web) struct DownloadsQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    lossless: String,
}

#[derive(Template)]
#[template(path = "downloads/page.html")]
struct DownloadsPage {
    live: Html,
    settled: Html,
}

impl Page for Downloads {
    const TAB: Tab = Tab::Downloads;
    const STREAM: &'static str = "/stream/downloads";
    type Args = View<DownloadsQuery>;

    async fn body(s: &UiState, View(q): View<DownloadsQuery>) -> anyhow::Result<Body> {
        if !q.q.trim().is_empty() {
            return Ok(Body::redirect(place_url(
                "/search",
                &[("q", &q.q), ("lossless", &q.lossless)],
            )));
        }
        let settled = Settled::newest(&s.app, crate::web::PAGE).await;
        let live = Live::of(&s.app, crate::web::PAGE, "", !settled.jobs.is_empty()).await;
        Ok(Body::of(&DownloadsPage {
            live: live.draw(),
            settled: settled.draw(),
        }))
    }
}

/// A job that landed or was cancelled, as its one line.
#[derive(PartialEq)]
pub(in crate::web) struct Done {
    id: String,
    /// What landed, as filed ("Artist — (Year) Album"), which is the
    /// confirmation a person wants; the query that asked for it otherwise.
    title: String,
    /// The album's page in the library, once it is filed.
    href: Option<String>,
    cancelled: bool,
}

impl From<crate::db::SettledJob> for Done {
    fn from(j: crate::db::SettledJob) -> Self {
        let title = match j.library_path.as_deref() {
            None => j.title,
            Some(path) => {
                let mut parts = path.rsplit('/');
                match (parts.next(), parts.next()) {
                    (Some(album), Some(artist)) => format!("{artist} — {album}"),
                    _ => path.to_string(),
                }
            }
        };
        let href = j
            .library_path
            .as_ref()
            .map(|path| place_url("/library", &[("album", path)]));
        Self {
            id: j.id.to_string(),
            title,
            href,
            cancelled: j.status == "cancelled",
        }
    }
}

/// The jobs done with: the long part of the list, none of which moves.
#[derive(Template, PartialEq)]
#[template(path = "downloads/settled.html")]
pub(in crate::web) struct Settled {
    /// Imported, or cancelled by someone: the newest shown.
    pub(in crate::web) jobs: Vec<Done>,
    /// Older jobs exist beyond those shown.
    more: bool,
    /// The database did not answer, so this is not the list as it stands.
    pub(in crate::web) failed: bool,
}

impl Settled {
    pub(in crate::web) async fn newest(app: &App, shown: i64) -> Self {
        let shown = shown.clamp(1, MOST);
        let read = crate::db::settled_jobs(&app.db, shown + 1)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "could not list settled jobs"));
        let failed = read.is_err();
        let mut jobs = read.unwrap_or_default();
        let more = shown < MOST && jobs.len() as i64 > shown;
        jobs.truncate(shown as usize);
        Self {
            jobs: jobs.into_iter().map(Done::from).collect(),
            more,
            failed,
        }
    }

    fn page(&self) -> i64 {
        crate::web::PAGE
    }
}

/// The jobs in hand: those waiting on a decision, then those on their way.
#[derive(Template)]
#[template(path = "downloads/live.html")]
pub(in crate::web) struct Live {
    /// Review, suspect, failed.
    pub(in crate::web) attention: Vec<Card>,
    /// Downloading, importing, or waiting on a metadata service.
    pub(in crate::web) underway: Vec<Card>,
    /// Nothing in hand and nothing settled either.
    empty: bool,
    /// The database did not answer, so this is not the list as it stands.
    pub(in crate::web) failed: bool,
}

impl Live {
    /// The newest `shown` jobs in hand, with the files drawn of those `files`
    /// names. `settled` is whether any job is done with, for the line an
    /// empty list shows.
    pub(in crate::web) async fn of(app: &App, shown: i64, files: &str, settled: bool) -> Self {
        let mut failed = false;
        let active = crate::db::active_jobs(&app.db, shown.clamp(1, MOST))
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "could not list jobs"))
            .unwrap_or_else(|_| {
                failed = true;
                Vec::new()
            });
        let jobs = graphql::job_views(app, active, true)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = ?e, "could not show jobs");
                failed = true;
                Vec::new()
            });
        let empty = jobs.is_empty() && !settled;
        let (attention, underway) = jobs
            .into_iter()
            .map(|j| {
                let open = files.split(',').any(|f| f == j.id.as_str());
                Card::of(app, j, false, open)
            })
            .partition(|c| c.warn);
        Self {
            attention,
            underway,
            empty,
            failed,
        }
    }

    /// The sections and the order of the cards in them: while this stays as
    /// it was, the cards can be sent one by one.
    pub(in crate::web) fn shape(&self) -> String {
        let ids = |cards: &[Card]| {
            cards
                .iter()
                .map(|c| c.j.id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{}|{}|{}",
            self.empty,
            ids(&self.attention),
            ids(&self.underway)
        )
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct UrlForm {
    url: String,
}

/// A download link, such as a Bandcamp purchase, fetched and imported.
pub(in crate::web) async fn import_url(
    State(s): State<UiState>,
    axum::Form(form): axum::Form<UrlForm>,
) -> Response {
    match s.app.jobs.from_url(&form.url).await {
        Ok(_) => accepted(),
        Err(e) => failed(&e),
    }
}

/// An action taken: any earlier error cleared and the page's question
/// closed. The stream draws what changed.
fn accepted() -> Response {
    askama_datastar::events([
        crate::web::flash::region().patch().into(),
        kit::Confirm::close().into(),
    ])
}

pub(in crate::web) async fn act(
    State(s): State<UiState>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Response {
    let jobs = &s.app.jobs;
    if action == "scam" {
        return match graphql::mark_scam(&s.app, id).await {
            Ok(scam) => {
                let moved = scam.jobs.iter().filter(|j| j.moved_to.is_some()).count();
                let msg = format!(
                    "Banned {}; {moved} of {} albums moved to another peer",
                    scam.peer,
                    scam.jobs.len()
                );
                askama_datastar::events([
                    crate::web::flash::ok(&msg).patch().into(),
                    kit::Confirm::close().into(),
                ])
            }
            Err(e) => failed(&anyhow::anyhow!("{}", e.message)),
        };
    }
    let result = match action.as_str() {
        "retry" => jobs.retry(id).await,
        "cancel" => jobs.cancel(id).await,
        "remove" => jobs.remove(id).await,
        "approve" => jobs.import_soon(id, true, None).await,
        "next" => jobs.next_source(id, crate::jobs::cause::REQUESTED).await,
        // Not waited for: filing waits behind any other import. The card
        // shows it importing, then filed, or back in review with the reason
        // it was refused.
        "as-is" => jobs.import_as_is_soon(id, sift::Edits::default()).await,
        "finish" => jobs.finish(id).await,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match result {
        Ok(()) => accepted(),
        Err(e) => failed(&e),
    }
}

/// One of the job's candidate releases, applied.
pub(in crate::web) async fn resolve(
    State(s): State<UiState>,
    Path((id, release)): Path<(Uuid, String)>,
) -> Response {
    let Some(release) = crate::library::release_id(&release) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => accepted(),
        Err(e) => failed(&e),
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct ReleaseForm {
    release: String,
}

/// A release named by hand, for when MusicBrainz or Discogs has it but the
/// matcher did not offer it. Takes the bare id or a pasted release link.
pub(in crate::web) async fn resolve_pasted(
    State(s): State<UiState>,
    Path(id): Path<Uuid>,
    axum::Form(form): axum::Form<ReleaseForm>,
) -> Response {
    let Some(release) = crate::library::release_id(&form.release) else {
        return failed(&anyhow::anyhow!(
            "That is not a MusicBrainz or Discogs release id or link."
        ));
    };
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => accepted(),
        Err(e) => failed(&e),
    }
}

/// Behind the session layer like everything else: a spectrogram is a
/// picture of someone's music.
pub(in crate::web) async fn spectrogram(
    State(s): State<UiState>,
    Path((id, n)): Path<(Uuid, u32)>,
) -> Response {
    match tokio::fs::read(s.app.jobs.spectrogram(id, n)).await {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "private, max-age=3600"),
            ],
            png,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A job's current source, the other copies it can move to, and its history.
#[derive(Template)]
#[template(path = "downloads/sources.html")]
pub(in crate::web) struct Sources {
    current: Option<SourceLine>,
    others: Vec<SourceLine>,
    events: Vec<crate::db::JobEvent>,
}

/// Where a job's files come from, or could.
struct SourceLine {
    /// The peer, or the host of a download link.
    from: String,
    /// The peer's profile; none for a link.
    profile: Option<String>,
    /// The folder, and where it is in the peer's shares.
    folder: Option<(String, String)>,
    /// Files and bytes the search found there; none when not recorded.
    found: Option<(usize, u64)>,
}

impl SourceLine {
    fn peer(username: &str, folder: Option<(&str, &[u8])>, found: Option<(usize, u64)>) -> Self {
        Self {
            from: username.to_string(),
            profile: Some(crate::web::profile_href(username)),
            folder: folder.map(|(f, raw)| {
                (
                    f.to_string(),
                    crate::web::browse_href(username, &super::browse::key(raw)),
                )
            }),
            found,
        }
    }
}

impl Sources {
    pub(in crate::web) fn new(
        job: Option<&crate::db::Job>,
        events: Vec<crate::db::JobEvent>,
    ) -> Self {
        use crate::db::Source;
        let current = job.map(|j| match &j.source.0 {
            Source::Soulseek {
                username,
                folder,
                folder_raw,
            } => SourceLine::peer(username, Some((folder, folder_raw)), None),
            Source::Files {
                username, folder, ..
            } => SourceLine::peer(username, folder.as_deref().map(|f| (f, f.as_bytes())), None),
            Source::Url { url } => SourceLine {
                from: crate::jobs::host(url),
                profile: None,
                folder: None,
                found: None,
            },
        });
        let others = job
            .map(|j| {
                j.alternates
                    .0
                    .iter()
                    .map(|a| {
                        let found = (!a.files.is_empty())
                            .then(|| (a.files.len(), a.files.iter().map(|f| f.1).sum()));
                        SourceLine::peer(&a.username, Some((&a.folder, &a.folder_raw)), found)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            current,
            others,
            events,
        }
    }

    pub(in crate::web) fn title(&self) -> String {
        self.events
            .last()
            .map_or_else(String::new, |e| e.title.clone())
    }

    fn outcome(&self, outcome: &str) -> &'static str {
        match outcome {
            "imported" => "imported",
            "review" => "held for a choice of release",
            "suspect" => "held to check quality",
            "failed" => "failed",
            "fallback" => "moved to another copy",
            "deferred" => "put off",
            _ => "other",
        }
    }
    fn cause(&self, cause: &str) -> &'static str {
        super::triage::cause_label(cause)
    }
    fn ago(&self, at: &chrono::DateTime<chrono::Utc>) -> String {
        crate::web::ago(*at)
    }
    fn size(&self, bytes: &u64) -> String {
        human(*bytes)
    }
}

/// A message in place of what a disclosure would have shown.
#[derive(Template)]
#[template(path = "downloads/trouble.html")]
struct Trouble {
    error: String,
}

/// A card's sources and history, read when it is opened.
pub(in crate::web) async fn sources(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    let (job, events) = tokio::join!(
        crate::db::job(&s.app.db, id),
        crate::db::job_events(&s.app.db, id)
    );
    match (job, events) {
        (Ok(job), Ok(events)) => Lazy::sources(
            &id.to_string(),
            job.as_ref().map_or(0, |j| j.alternates.0.len()),
        )
        .answer(Sources::new(job.as_ref(), events).markup()),
        (Err(e), _) | (_, Err(e)) => Lazy::sources(&id.to_string(), 0).answer(
            Trouble {
                error: format!("Couldn't read the history: {e:#}"),
            }
            .markup(),
        ),
    }
}

/// A job's files and the tags they carry, read when its card asks: the
/// evidence for a choice, which the card itself does not hold.
#[derive(Template)]
#[template(path = "downloads/files.html")]
struct Files {
    error: Option<String>,
    /// Each album the tags name, with a MusicBrainz search for it.
    albums: Vec<(String, String)>,
    tracks: Vec<sift::meta::Track>,
}

impl Files {
    fn position(&self, t: &sift::meta::Track) -> String {
        match (t.disc, t.track) {
            (Some(d), Some(n)) if t.disc_total.unwrap_or(d) > 1 => format!("{d}-{n}"),
            (_, Some(n)) => n.to_string(),
            _ => "?".into(),
        }
    }
    fn length(&self, t: &sift::meta::Track) -> String {
        let s = t.duration.as_secs();
        format!("{}:{:02}", s / 60, s % 60)
    }
    fn quality(&self, t: &sift::meta::Track) -> String {
        let mut q = t.format.clone();
        if let Some(b) = t.bit_depth {
            q += &format!(" {b}-bit");
        }
        if let Some(r) = t.sample_rate {
            q += &format!(" {} kHz", f64::from(r) / 1000.0);
        }
        q
    }
    fn name(&self, t: &sift::meta::Track) -> String {
        t.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

pub(in crate::web) async fn files(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    let (tracks, error) = match s.app.jobs.tracks(id).await {
        Ok(mut t) => {
            t.sort_by_key(|t| (t.disc, t.track, t.path.clone()));
            (t, None)
        }
        Err(e) => (Vec::new(), Some(format!("Couldn't read the files: {e:#}"))),
    };
    let mut albums: Vec<(String, String)> = Vec::new();
    for t in &tracks {
        let artist = t.album_artist.as_ref().or(t.artist.as_ref());
        let shown = match (artist, &t.album) {
            (Some(a), Some(b)) => format!("{a} — {b}"),
            (None, Some(b)) => b.clone(),
            _ => continue,
        };
        if albums.iter().any(|(s, _)| *s == shown) {
            continue;
        }
        let query = [artist.cloned(), t.album.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let search = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("query", &query)
            .append_pair("type", "release")
            .append_pair("method", "indexed")
            .finish();
        albums.push((shown, format!("https://musicbrainz.org/search?{search}")));
    }
    Lazy::files(&id.to_string()).answer(
        Files {
            error,
            albums,
            tracks,
        }
        .markup(),
    )
}

/// The files of a downloading job, each ticked to fetch or not.
#[derive(Template)]
#[template(path = "downloads/pick.html")]
struct Pick {
    id: Uuid,
    error: Option<String>,
    files: Vec<graphql::JobFile>,
}

impl Pick {
    async fn of(app: &App, id: Uuid, error: Option<String>) -> Self {
        let files = match crate::db::job(&app.db, id).await {
            Ok(Some(job)) => graphql::job_view(app, job, true)
                .await
                .map(|j| j.files)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Self { id, error, files }
    }
    fn size(&self, n: &u64) -> String {
        human(*n)
    }
}

pub(in crate::web) async fn pick(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    Lazy::pick(&id.to_string()).answer(Pick::of(&s.app, id, None).await.markup())
}

/// The ticked files are fetched and the rest skipped.
pub(in crate::web) async fn pick_files(
    State(s): State<UiState>,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> Response {
    let form = fields(&body);
    // Only the files the form showed: a source changed since it was drawn
    // brings files nobody has chosen about.
    let named = |name: &str| -> std::collections::HashSet<String> {
        form.iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let want = named("want");
    let skip = named("shown").difference(&want).cloned().collect();
    let error = s
        .app
        .jobs
        .skip_files(id, &skip, &want)
        .await
        .err()
        .map(|e| format!("{e:#}"));
    let pick = Pick::of(&s.app, id, error).await.markup();
    Lazy::pick(&id.to_string()).answer(pick)
}

/// A job's files against one release, track by track, as beets shows a
/// match before it is taken.
#[derive(Template)]
#[template(path = "downloads/compare.html")]
struct Compare {
    error: Option<String>,
    cmp: Option<sift::Comparison>,
}

impl Compare {
    fn dist(&self, d: &f64) -> String {
        format!("{d:.2}")
    }
    fn delta(&self, d: &f64) -> String {
        format!("{d:+.0}s")
    }
}

pub(in crate::web) async fn compare(
    State(s): State<UiState>,
    Path((id, release)): Path<(Uuid, String)>,
) -> Response {
    let Some(release) = crate::library::release_id(&release) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (cmp, error) = match s.app.jobs.compare(id, &release).await {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(format!("Couldn't compare: {e:#}"))),
    };
    Lazy::compare(&id.to_string(), &release).answer(Compare { error, cmp }.markup())
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use uuid::Uuid;

    use super::Sources;
    use super::job::of_path as job_of_path;
    use crate::web::browse_href;

    #[test]
    fn a_job_page_is_named_by_its_id_alone() {
        let id = Uuid::new_v4();
        assert_eq!(job_of_path(&format!("/jobs/{id}")), Some(id));
        assert_eq!(job_of_path(&format!("/jobs/{id}/files")), None);
        assert_eq!(job_of_path("/jobs/nope"), None);
    }

    #[test]
    fn sources_link_folders_by_their_bytes_and_skip_unrecorded_counts() {
        use crate::db::{Alternate, Job as Row, Source};
        let raw = b"music\\caf\xe9".to_vec();
        let row = Row::new(
            "me".into(),
            "a".into(),
            Source::Soulseek {
                username: "peer".into(),
                folder: String::from_utf8_lossy(&raw).into_owned(),
                folder_raw: raw.clone(),
            },
            vec![
                Alternate {
                    username: "old".into(),
                    folder: "x".into(),
                    folder_raw: b"x".to_vec(),
                    files: Vec::new(),
                },
                Alternate {
                    username: "new".into(),
                    folder: "y".into(),
                    folder_raw: b"y".to_vec(),
                    files: vec![(b"y\\1.flac".to_vec(), 10), (b"y\\2.flac".to_vec(), 5)],
                },
            ],
        );
        let view = Sources::new(Some(&row), Vec::new());
        let current = view.current.as_ref().unwrap();
        assert_eq!(
            current.folder.as_ref().unwrap().1,
            browse_href("peer", &URL_SAFE_NO_PAD.encode(&raw))
        );
        let found: Vec<_> = view.others.iter().map(|o| o.found).collect();
        assert_eq!(found, [None, Some((2, 15))]);
        assert!(
            askama::Template::render(&view)
                .unwrap()
                .contains("2 files · 15 B")
        );
    }
}
