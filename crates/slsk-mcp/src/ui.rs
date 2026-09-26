//! The web UI: server-rendered HTML patched over SSE by Datastar.
//!
//! One page. A stream pushes the status line, the jobs and the transfers
//! once a second; searching streams its own results as peers answer. Every
//! route but sign-in, the probe and the static assets sits behind the
//! session layer, which wraps the router whole so a route added later is
//! protected by construction.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::Stream;
use slsk_engine::slsk_proto::RawStr;
use uuid::Uuid;

use crate::App;
use crate::auth::session::Sessions;
use crate::config::Config;
use crate::folders::{Filter, Folder};
use crate::graphql::{self, Job, Transfer};

#[derive(Clone)]
pub struct UiState {
    pub app: Arc<App>,
    pub config: Arc<Config>,
    pub sessions: Sessions,
    pub http: reqwest::Client,
}

const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

pub fn router(app: Arc<App>) -> Router {
    let state = UiState {
        config: app.cfg.clone(),
        app,
        sessions: Sessions::new(),
        http: reqwest::Client::new(),
    };
    let protected = Router::new()
        .route("/", get(page))
        .route("/stream", get(stream))
        .route("/search", post(search))
        .route("/grab", post(grab))
        .route("/download", post(download))
        .route("/jobs/{id}/{action}", post(job_action))
        .route("/jobs/{id}/resolve/{release}", post(resolve))
        .route("/jobs/{id}/spectrogram/{n}", get(spectrogram))
        .route("/uploads/cancel", post(cancel_upload))
        .route("/ban", post(ban))
        .route("/account", post(account))
        .route("/reconnect", post(reconnect))
        .layer(axum::middleware::from_fn(require_datastar_on_post))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::extract::require_session,
        ));
    Router::new()
        .merge(protected)
        .merge(crate::auth::routes::router())
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/assets/datastar.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_bytes!("../assets/datastar.js").as_slice(),
                )
            }),
        )
        .route(
            "/assets/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_bytes!("../assets/app.css").as_slice(),
                )
            }),
        )
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// Every state change arrives from Datastar, which marks its requests with a
/// header a cross-site form cannot set. Refusing POSTs without it closes
/// cross-site request forgery independently of the cookie's SameSite.
async fn require_datastar_on_post(request: Request, next: Next) -> Response {
    if request.method() == http::Method::POST && !request.headers().contains_key("datastar-request")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn patch(html: &str) -> Event {
    Event::default().event("datastar-patch-elements").data(
        html.lines()
            .map(|l| format!("elements {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn sse(
    events: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
}

fn one(html: String) -> Response {
    sse(futures_util::stream::once(async move { Ok(patch(&html)) })).into_response()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn percent(done: u64, total: u64) -> u64 {
    (done * 100).checked_div(total).unwrap_or(0).min(100)
}

#[derive(Template)]
#[template(path = "page.html")]
struct Page {
    status: String,
    jobs: String,
    transfers: String,
}

async fn page(State(s): State<UiState>) -> Result<Html<String>, crate::error::AppError> {
    let page = Page {
        status: status_html(&s.app).await,
        jobs: jobs_html(&s.app).await,
        transfers: transfers_html(&s.app),
    };
    Ok(Html(page.render().map_err(anyhow::Error::from)?))
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusView {
    state: &'static str,
    username: Option<String>,
    detail: Option<String>,
    files: usize,
    up: String,
    down: String,
    children: usize,
}

async fn status_html(app: &App) -> String {
    let view = match app.session.engine() {
        None => StatusView {
            state: "no-account",
            username: None,
            detail: None,
            files: 0,
            up: String::new(),
            down: String::new(),
            children: 0,
        },
        Some(e) => {
            use slsk_engine::Status as S;
            let (state, detail) = match &*e.status().borrow() {
                S::Connecting => ("connecting", None),
                S::LoggedIn { .. } => ("connected", None),
                S::Rejected { reason } => ("rejected", Some(reason.clone())),
                S::Displaced => (
                    "displaced",
                    Some("another client logged in with this account".to_string()),
                ),
                S::Disconnected { error } => ("disconnected", Some(error.clone())),
            };
            let speed = |v: Vec<slsk_engine::TransferView>| {
                human(v.iter().map(|t| t.speed).sum::<u64>()) + "/s"
            };
            StatusView {
                state,
                username: Some(e.username()),
                detail,
                files: e.share_counts().1,
                up: speed(e.uploads()),
                down: speed(e.downloads()),
                children: e.distributed().3,
            }
        }
    };
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsView {
    /// Waiting on a decision: review, suspect, failed.
    attention: Vec<Job>,
    /// Downloading or importing.
    underway: Vec<Job>,
    /// Imported, or cancelled by someone.
    settled: Vec<Job>,
}

impl JobsView {
    fn verdict(&self, t: &crate::analysis::TrackAnalysis) -> &'static str {
        use crate::analysis::Verdict as V;
        match t.verdict {
            V::Lossless => "lossless",
            V::Lossy => "lossy",
            V::Upsampled => "upsampled",
            V::Uncertain => "uncertain",
            V::Unknown => "unknown",
        }
    }
    fn pct(&self, j: &Job) -> u64 {
        percent(j.downloaded_bytes, j.total_bytes)
    }
    /// The signal that is true while a request from this job's card is in
    /// flight. Local (leading underscore), so it is never sent back.
    fn sig(&self, j: &Job) -> String {
        format!("_busy_{}", j.id.as_str().replace('-', ""))
    }
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
    /// What landed, as filed ("Artist — (Year) Album"), which is the
    /// confirmation a person wants; the query that asked for it otherwise.
    fn landed(&self, j: &Job) -> String {
        let Some(path) = j.library_path.as_deref() else {
            return j.title.clone();
        };
        let mut parts = path.rsplit('/');
        match (parts.next(), parts.next()) {
            (Some(album), Some(artist)) => format!("{artist} — {album}"),
            _ => path.to_string(),
        }
    }
    /// What the job needs from the person, in their terms; the reason
    /// underneath is the machine's.
    fn ask(&self, j: &Job) -> Option<&'static str> {
        match j.status.as_str() {
            "review" if j.candidates.is_empty() => {
                Some("MusicBrainz has nothing that fits these files. Try another copy.")
            }
            "review" => Some(
                "Not sure which release this is. Pick the one that matches, or try another copy.",
            ),
            "suspect" => Some(
                "Some tracks may not be true lossless. Check the spectrograms, then import anyway or try another copy.",
            ),
            "failed" => Some("This one could not finish."),
            _ => None,
        }
    }
}

async fn jobs_html(app: &App) -> String {
    let mut jobs = Vec::new();
    match crate::db::jobs(&app.db, None, 40).await {
        Ok(rows) => {
            for j in rows {
                match graphql::job_view(app, j, false).await {
                    Ok(v) => jobs.push(v),
                    Err(e) => tracing::warn!(error = ?e, "could not show a job"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not list jobs"),
    }
    let mut view = JobsView {
        attention: Vec::new(),
        underway: Vec::new(),
        settled: Vec::new(),
    };
    for j in jobs {
        match j.status.as_str() {
            "review" | "suspect" | "failed" => view.attention.push(j),
            "imported" | "cancelled" => view.settled.push(j),
            _ => view.underway.push(j),
        }
    }
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "transfers.html")]
struct TransfersView {
    downloads: Vec<Transfer>,
    uploads: Vec<Transfer>,
}

impl TransfersView {
    fn pct(&self, t: &Transfer) -> u64 {
        percent(t.bytes, t.size)
    }
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
    /// Short enough for a phone's last column.
    fn label(&self, t: &Transfer) -> String {
        match (t.state.as_str(), t.place) {
            ("remote_queued" | "queued", Some(p)) => format!("queued #{p}"),
            ("remote_queued" | "queued", None) => "queued".into(),
            ("transferring", _) => "active".into(),
            ("completed", _) => "done".into(),
            (s, _) => s.into(),
        }
    }
    fn short<'a>(&self, name: &'a str) -> &'a str {
        name.rsplit('\\').next().unwrap_or(name)
    }
}

fn transfers_html(app: &App) -> String {
    let (downloads, uploads) = match app.session.engine() {
        Some(e) => {
            let active = |v: Vec<slsk_engine::TransferView>| -> Vec<Transfer> {
                v.into_iter()
                    .filter(|t| !matches!(t.state, "completed" | "cancelled"))
                    .rev()
                    .take(100)
                    .map(Transfer::from)
                    .collect()
            };
            (active(e.downloads()), active(e.uploads()))
        }
        None => (Vec::new(), Vec::new()),
    };
    TransfersView { downloads, uploads }
        .render()
        .unwrap_or_default()
}

/// Re-render the live parts once a second. The stream ends itself after a
/// while and the page reopens it on an interval: a stream can die without
/// either end noticing (a suspended phone, a replaced pod), and reopening on
/// a timer is what bounds how long a page can sit showing stale state.
async fn stream(State(s): State<UiState>) -> impl IntoResponse {
    let app = s.app.clone();
    let events = async_stream::stream! {
        for _ in 0..20 {
            yield Ok(patch(&status_html(&app).await));
            yield Ok(patch(&jobs_html(&app).await));
            yield Ok(patch(&transfers_html(&app)));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    sse(events)
}

#[derive(serde::Deserialize)]
struct SearchForm {
    q: String,
    #[serde(default)]
    lossless: bool,
}

#[derive(Template)]
#[template(path = "results.html")]
struct ResultsView {
    query: String,
    folders: Vec<Folder>,
    searching: bool,
    responses: usize,
}

impl ResultsView {
    fn key(&self, f: &Folder) -> String {
        URL_SAFE_NO_PAD.encode(f.remote_path.as_bytes())
    }
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
    fn quality(&self, f: &Folder) -> String {
        match (f.lossless, f.bit_depth, f.sample_rate, f.bitrate) {
            (true, Some(d), Some(r), _) => format!("{d}/{}", r as f64 / 1000.0),
            (true, ..) => "lossless".into(),
            (false, _, _, Some(b)) => format!("{b} kbps"),
            _ => "lossy".into(),
        }
    }
}

/// Stream results as peers answer, regrouped every second or so.
async fn search(State(s): State<UiState>, axum::Form(form): axum::Form<SearchForm>) -> Response {
    let Some(engine) = s.app.session.engine().cloned() else {
        return one(r#"<div id="results" class="note">Sign in to Soulseek first.</div>"#.into());
    };
    let query = form.q.trim().to_string();
    let filter = Filter {
        lossless: form.lossless,
        ..Default::default()
    };
    let rx = engine.search(&query);
    let events = async_stream::stream! {
        let Ok(mut rx) = rx else {
            yield Ok(patch(r#"<div id="results" class="note">Not connected.</div>"#));
            return;
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        let mut responses = Vec::new();
        loop {
            let tick = tokio::time::Instant::now() + Duration::from_millis(1000);
            while let Ok(Some(r)) = tokio::time::timeout_at(tick.min(deadline), rx.recv()).await {
                responses.push(r);
            }
            let done = tokio::time::Instant::now() >= deadline;
            let folders = crate::folders::relevant(crate::folders::group(&responses, &filter), &query).into_iter().take(60).collect();
            let view = ResultsView { query: query.clone(), folders, searching: !done, responses: responses.len() };
            yield Ok(patch(&view.render().unwrap_or_default()));
            if done {
                break;
            }
        }
    };
    sse(events).into_response()
}

#[derive(serde::Deserialize)]
struct DownloadForm {
    username: String,
    key: String,
    title: Option<String>,
}

async fn grab(State(s): State<UiState>, axum::Form(form): axum::Form<SearchForm>) -> Response {
    let filter = form.lossless.then(|| Filter {
        lossless: true,
        ..Default::default()
    });
    match graphql::grab(&s.app, form.q.trim(), 10, filter).await {
        Ok(_) => one(jobs_html(&s.app).await),
        Err(e) => one(format!(
            r#"<div id="flash" class="flash error">{}</div>"#,
            askama_escape(&e.message)
        )),
    }
}

async fn download(
    State(s): State<UiState>,
    axum::Form(form): axum::Form<DownloadForm>,
) -> Response {
    let Ok(raw) = URL_SAFE_NO_PAD.decode(&form.key) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let remote = RawStr(raw.into());
    let folder = Folder {
        username: form.username,
        path: remote.to_string_lossy(),
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
    match s
        .app
        .jobs
        .from_folder(&folder, form.title, Vec::new())
        .await
    {
        Ok(_) => one(jobs_html(&s.app).await),
        Err(e) => one(format!(
            r#"<div id="flash" class="flash error">{}</div>"#,
            askama_escape(&format!("{e:#}"))
        )),
    }
}

async fn job_action(
    State(s): State<UiState>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Response {
    let jobs = &s.app.jobs;
    let result = match action.as_str() {
        "retry" => jobs.retry(id).await,
        "cancel" => jobs.cancel(id).await,
        "remove" => jobs.remove(id).await,
        "approve" => jobs.import_soon(id, true, None).await,
        "next" => jobs.next_source(id).await,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match result {
        Ok(()) => done(&s.app).await,
        Err(e) => failed(&e),
    }
}

/// The job list as it now stands, and any earlier error cleared: the change
/// on the card is the confirmation.
async fn done(app: &App) -> Response {
    one(format!(
        "{}\n<div id=\"flash\"></div>",
        jobs_html(app).await
    ))
}

fn failed(e: &anyhow::Error) -> Response {
    one(format!(
        r#"<div id="flash" class="flash error" role="alert">{}</div>"#,
        askama_escape(&format!("{e:#}"))
    ))
}

async fn resolve(State(s): State<UiState>, Path((id, release)): Path<(Uuid, String)>) -> Response {
    if Uuid::parse_str(&release).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => done(&s.app).await,
        Err(e) => failed(&e),
    }
}

/// Behind the session layer like everything else: a spectrogram is a
/// picture of someone's music.
async fn spectrogram(State(s): State<UiState>, Path((id, n)): Path<(Uuid, u32)>) -> Response {
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

#[derive(serde::Deserialize)]
struct UserFileForm {
    username: String,
    filename: Option<String>,
}

async fn cancel_upload(
    State(s): State<UiState>,
    axum::Form(f): axum::Form<UserFileForm>,
) -> Response {
    if let (Some(e), Some(name)) = (s.app.session.engine(), f.filename) {
        e.cancel_upload(&f.username, &RawStr::from(name));
    }
    one(transfers_html(&s.app))
}

async fn ban(State(s): State<UiState>, axum::Form(f): axum::Form<UserFileForm>) -> Response {
    if let Some(e) = s.app.session.engine() {
        let account = e.username();
        if crate::db::set_ban(&s.app.db, &account, &f.username, true)
            .await
            .is_ok()
            && let Ok(bans) = crate::db::bans(&s.app.db, &account).await
        {
            e.set_banned(bans);
        }
    }
    one(transfers_html(&s.app))
}

#[derive(serde::Deserialize)]
struct AccountForm {
    username: String,
    password: String,
}

async fn account(State(s): State<UiState>, axum::Form(f): axum::Form<AccountForm>) -> Response {
    let (u, p) = (f.username.trim(), f.password.as_str());
    if u.is_empty() || p.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match s.app.session.use_account(u, p).await {
        Ok(_) => {
            let _ = s.app.jobs.resume().await;
            one(status_html(&s.app).await)
        }
        Err(e) => one(format!(
            r#"<div id="flash" class="flash error">{}</div>"#,
            askama_escape(&format!("{e:#}"))
        )),
    }
}

async fn reconnect(State(s): State<UiState>) -> Response {
    if let Some(e) = s.app.session.engine() {
        e.reconnect();
    }
    one(status_html(&s.app).await)
}

fn askama_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
