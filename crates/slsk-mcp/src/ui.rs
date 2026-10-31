//! The web UI: server-rendered HTML patched over SSE by Datastar.
//!
//! A page per address, drawn whole on the server around a shared layout;
//! Datastar patches fragments of it in place. A stream pushes the status
//! line and the page's own live parts once a second, each only when it
//! changed; searching streams its own results as peers answer. Every
//! route but sign-in, the probe and the static assets sits behind the
//! session layer, which wraps the router whole so a route added later is
//! protected by construction.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, RawQuery, Request, State};
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

mod admin;
mod browse;
mod chat;
mod library;
mod profile;
#[cfg(test)]
mod tests;
mod wishlist;

#[derive(Clone)]
pub struct UiState {
    pub app: Arc<App>,
    pub config: Arc<Config>,
    pub sessions: Sessions,
    pub http: reqwest::Client,
    /// The library action running in the background, or the last one.
    run: Arc<parking_lot::Mutex<library::LibraryRun>>,
    /// Album art previewed and not yet applied.
    art: library::ArtStash,
    browsed: browse::Cache,
    profiles: profile::Cache,
}

const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

pub fn router(app: Arc<App>) -> Router {
    let state = UiState {
        config: app.cfg.clone(),
        sessions: Sessions::new(app.db.clone()),
        app,
        http: reqwest::Client::new(),
        run: Default::default(),
        art: Default::default(),
        browsed: Default::default(),
        profiles: Default::default(),
    };
    let protected = Router::new()
        // Drawn whole by `pages`; nothing fetches a fragment of them.
        .route("/", get(StatusCode::NOT_FOUND))
        .route("/uploads", get(StatusCode::NOT_FOUND))
        .route("/stream", get(stream))
        .route("/search", get(search))
        .route("/grab", post(grab))
        .route("/import-url", post(import_url))
        .route("/download", post(download))
        .route("/jobs/{id}/{action}", post(job_action))
        .route("/jobs/{id}/resolve/{release}", post(resolve))
        .route("/jobs/{id}/release", post(resolve_pasted))
        .route("/jobs/{id}/spectrogram/{n}", get(spectrogram))
        .route("/jobs/{id}/files", get(job_files))
        .route("/jobs/{id}/compare/{release}", get(job_compare))
        .route("/uploads/cancel", post(cancel_upload))
        .route("/ban", post(ban))
        .route("/account", post(account))
        .route("/reconnect", post(reconnect))
        .merge(wishlist::routes())
        .merge(library::routes())
        .merge(browse::routes())
        .merge(profile::routes())
        .merge(chat::routes())
        .merge(admin::routes())
        .layer(axum::middleware::from_fn(require_datastar_on_post))
        .layer(axum::middleware::from_fn_with_state(state.clone(), pages))
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
            get(|q: RawQuery| async { asset(DATASTAR_JS, "text/javascript; charset=utf-8", q) }),
        )
        .route(
            "/assets/app.css",
            get(|q: RawQuery| async { asset(APP_CSS, "text/css; charset=utf-8", q) }),
        )
        .route(
            "/assets/icon-32.png",
            get(|| async { png(include_bytes!("../assets/icon-32.png")) }),
        )
        .route(
            "/assets/icon-192.png",
            get(|| async { png(include_bytes!("../assets/icon-192.png")) }),
        )
        .route(
            "/assets/apple-touch-icon.png",
            get(|| async { png(include_bytes!("../assets/apple-touch-icon.png")) }),
        )
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

const APP_CSS: &str = include_str!("../assets/app.css");
const DATASTAR_JS: &str = include_str!("../assets/datastar.js");

/// `path` with a hash of `body`, the asset served there, so a page from a
/// new build fetches its own stylesheet and script rather than a cached copy
/// of the previous build's.
fn versioned(path: &str, body: &str) -> String {
    format!("{path}?v={}", fingerprint(body.as_bytes()))
}

/// A short hash of `bytes` (FNV-1a) for an address that changes with them.
fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

static CSS_URL: LazyLock<String> = LazyLock::new(|| versioned("/assets/app.css", APP_CSS));
static DATASTAR_URL: LazyLock<String> =
    LazyLock::new(|| versioned("/assets/datastar.js", DATASTAR_JS));

/// A text asset. Requested by its hashed URL (`?v=`), it never changes, so it
/// is kept for good; a bare URL is rechecked hourly.
fn asset(body: &'static str, kind: &'static str, query: RawQuery) -> Response {
    let cache = if query.0.is_some_and(|q| q.starts_with("v=")) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}

/// Public like the other assets: a browser asks for the icon before anyone signs in.
fn png(bytes: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], bytes)
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

/// A bare `\r` would start an SSE data line of its own, outside the
/// `elements` prefix, so it is dropped before the split.
fn patch(html: &str) -> Event {
    Event::default().event("datastar-patch-elements").data(
        html.replace('\r', "")
            .lines()
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

/// Elements added at the end of the one `selector` names: rows streamed
/// into a list as each is worked out.
fn append(selector: &str, html: &str) -> Event {
    insert(selector, "append", html)
}

/// Elements added at the start of the one `selector` names.
fn prepend(selector: &str, html: &str) -> Event {
    insert(selector, "prepend", html)
}

fn insert(selector: &str, mode: &str, html: &str) -> Event {
    Event::default().event("datastar-patch-elements").data(
        [format!("selector {selector}"), format!("mode {mode}")]
            .into_iter()
            .chain(
                html.replace('\r', "")
                    .lines()
                    .map(|l| format!("elements {l}")),
            )
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Several events in one response: fragments, then signals to reset.
fn many(events: Vec<Event>) -> Response {
    sse(futures_util::stream::iter(events.into_iter().map(Ok))).into_response()
}

/// Set signals from the server, as JSON: how a form is cleared once what it
/// sent has been accepted.
fn signals(json: &str) -> Event {
    Event::default()
        .event("datastar-patch-signals")
        .data(format!("signals {json}"))
}

/// A confirmation that fades by itself; tapping it dismisses it sooner.
fn flash_ok(msg: &str) -> String {
    format!(
        r#"<div id="flash" class="toast border-brand text-brand animate-fade motion-reduce:[animation-timing-function:steps(1,end)]" role="status" data-on:click="el.className = ''">{}</div>"#,
        escape(msg)
    )
}

fn flash_err(msg: &str) -> String {
    format!(
        r#"<div id="flash" class="toast border-bad text-meta text-bad" role="alert" data-on:click="el.className = ''">{}</div>"#,
        escape(msg)
    )
}

/// A form body whose fields may repeat (checkboxes, several hidden paths),
/// which `axum::Form` does not collect.
fn fields(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds().max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// A job's status as a person would say it.
fn status_label(status: &str) -> &'static str {
    match status {
        "review" => "needs a choice",
        "suspect" => "check quality",
        "importing" => "filing",
        "downloading" => "downloading",
        "imported" => "in library",
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "working",
    }
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

/// The pages, in the order the nav lists them: name, address and label.
const PAGES: &[(&str, &str, &str)] = &[
    ("search", "/search", "Search"),
    ("albums", "/", "Albums"),
    ("uploads", "/uploads", "Uploads"),
    ("wishlist", "/wishlist", "Wishlist"),
    ("library", "/library", "Library"),
    ("browse", "/browse", "Browse"),
    ("chat", "/chat", "Chat"),
    ("profile", "/profile", "Profile"),
    ("bans", "/bans", "Bans"),
    ("settings", "/settings", "Settings"),
    ("triage", "/triage", "Triage"),
];

/// The layout every page shares: the status line, the nav and the live
/// stream, around the page's own body.
#[derive(Template)]
#[template(path = "page.html")]
struct Page {
    status: String,
    up_count: String,
    chat_count: String,
    version: &'static str,
    /// Which page this is, as `PAGES` names it.
    tab: &'static str,
    /// The page's starting signals, from the address.
    signals: String,
    body: String,
}

struct NavItem {
    name: &'static str,
    href: &'static str,
    label: &'static str,
    active: bool,
}

impl Page {
    fn nav(&self) -> Vec<NavItem> {
        PAGES
            .iter()
            .map(|&(name, href, label)| NavItem {
                name,
                href,
                label,
                active: name == self.tab,
            })
            .collect()
    }
    fn css(&self) -> &str {
        &CSS_URL
    }
    fn datastar(&self) -> &str {
        &DATASTAR_URL
    }
    fn title(&self) -> &'static str {
        PAGES
            .iter()
            .find(|p| p.0 == self.tab)
            .map_or("slsk", |p| p.2)
    }
}

#[derive(Template)]
#[template(path = "pages/albums.html")]
struct AlbumsPage {
    jobs: String,
}

#[derive(Template)]
#[template(path = "pages/search.html")]
struct SearchPage {
    q: String,
    lossless: bool,
}

/// A browser asking for a page's address gets the whole page. The same
/// address fetched by the page itself (`Datastar-Request`) is a fragment of
/// it, and goes on to its own handler.
async fn pages(State(s): State<UiState>, request: Request, next: Next) -> Response {
    if request.method() == http::Method::GET
        && !request.headers().contains_key("datastar-request")
        && let Some(&(tab, ..)) = PAGES.iter().find(|p| request.uri().path() == p.1)
    {
        // Searches were once drawn on Albums, at `/?q=…`.
        if tab == "albums"
            && let Some(query) = request.uri().query()
            && url::form_urlencoded::parse(query.as_bytes()).any(|(k, v)| k == "q" && !v.is_empty())
        {
            return axum::response::Redirect::to(&format!("/search?{query}")).into_response();
        }
        return match render_page(&s, tab, request.uri()).await {
            Ok(html) => html.into_response(),
            Err(e) => e.into_response(),
        };
    }
    next.run(request).await
}

/// A user's shares at a folder (the top when `key` is empty).
fn browse_href(username: &str, key: &str) -> String {
    place_url("/browse", &[("username", username), ("key", key)])
}

/// A user's profile; this account's own when `username` is empty.
fn profile_href(username: &str) -> String {
    place_url("/profile", &[("username", username)])
}

/// A conversation: `kind` is `room` or `pm`.
fn chat_href(kind: &str, name: &str) -> String {
    place_url("/chat", &[("kind", kind), ("name", name)])
}

/// A link's click handler: a modified click (a new tab, a download) is left
/// to the browser; a plain one runs `then` in place, unless `busy` (a signal,
/// or empty for none) says the last is still loading.
fn follow(busy: impl std::fmt::Display, then: impl std::fmt::Display) -> String {
    let busy = busy.to_string();
    let then = if busy.is_empty() {
        then.to_string()
    } else {
        format!("{busy} || ({then})")
    };
    format!(
        "evt.metaKey || evt.ctrlKey || evt.shiftKey || evt.altKey || (evt.preventDefault(), {then})"
    )
}

/// An address with its query, as the page keeps it.
fn place_url(path: &str, params: &[(&str, &str)]) -> String {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().filter(|(_, v)| !v.is_empty()))
        .finish();
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

/// Where the page now is, for the address bar. Every action that moves
/// within a page reports it, so a reload, a shared link or back and forward
/// return to the same place: the address is the one that draws it.
fn place(url: &str) -> Event {
    signals(&serde_json::json!({ "_url": url }).to_string())
}

/// The view an address describes; one that does not parse (a repeated key,
/// a value of the wrong type) is the default view rather than an error.
fn query<T: serde::de::DeserializeOwned + Default>(uri: &http::Uri) -> T {
    axum_extra::extract::Query::try_from_uri(uri).map_or_else(|_| T::default(), |q| q.0)
}

async fn render_page(
    s: &UiState,
    tab: &'static str,
    uri: &http::Uri,
) -> Result<Html<String>, crate::error::AppError> {
    let url = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    let search: SearchQuery = match tab {
        "search" => query(uri),
        _ => SearchQuery::default(),
    };
    let lossless = search.q.is_empty() || !search.lossless.is_empty();
    let filter: library::Filter = match tab {
        "library" => query(uri),
        _ => library::Filter::default(),
    };
    let signals = serde_json::json!({
        "q": search.q,
        "lossless": lossless,
        "busy": false,
        "jobs": PAGE,
        "history": PAGE,
        "_url": url,
        "_libq": filter.q,
        "_libsince": filter.since,
        "_libfrom": filter.from,
        "_libshow": filter.show,
        "_libalbum": filter.album,
    })
    .to_string();
    let body = match tab {
        "albums" => AlbumsPage {
            jobs: jobs_html(&s.app, PAGE).await,
        }
        .render()
        .unwrap_or_default(),
        "search" => SearchPage {
            q: search.q,
            lossless,
        }
        .render()
        .unwrap_or_default(),
        "uploads" => uploads_html(&s.app, PAGE).await,
        "wishlist" => wishlist::page(&s.app).await,
        "library" => library::page(s, &filter),
        "browse" => browse::page(s, &query::<browse::BrowseQuery>(uri).username),
        "chat" => chat::page(&s.app, &query(uri)).await,
        "profile" => profile::page(s, &query::<profile::ProfileQuery>(uri).username),
        "bans" => admin::bans_page(&s.app).await,
        "settings" => admin::settings_page(&s.app).await,
        _ => admin::triage_page(&s.app, &query(uri)).await,
    };
    let page = Page {
        status: status_html(&s.app).await,
        up_count: up_count_html(&s.app),
        chat_count: chat::count_html(&s.app).await,
        version: env!("CARGO_PKG_VERSION"),
        tab,
        signals,
        body,
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

/// Rows shown at first, and added by each "Show more", in the lists that
/// grow without end: each kind of job, and upload history. A phone browser
/// cannot hold many thousands of them.
const PAGE: i64 = 500;

/// `PAGE`, for a form that does not say how many rows its page shows.
fn page() -> i64 {
    PAGE
}

/// A shown count from a form, where an empty or unreadable value (a hidden
/// input whose signal has not been applied) is `PAGE`.
fn shown_field<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(<String as serde::Deserialize>::deserialize(d)?
        .parse()
        .unwrap_or(PAGE))
}

/// The most rows a page may ask for, whatever its signals say: ten pages.
const MOST: i64 = 5_000;

/// A job that landed or was cancelled, as its one line.
struct Settled {
    id: String,
    /// What landed, as filed ("Artist — (Year) Album"), which is the
    /// confirmation a person wants; the query that asked for it otherwise.
    title: String,
    /// The album's page in the library, once it is filed.
    href: Option<String>,
    cancelled: bool,
}

impl From<crate::db::Job> for Settled {
    fn from(j: crate::db::Job) -> Self {
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
        let href = j.library_path.as_ref().map(|path| {
            library::Filter {
                album: path.clone(),
                ..Default::default()
            }
            .url()
        });
        Self {
            id: j.id.to_string(),
            title,
            href,
            cancelled: j.status == "cancelled",
        }
    }
}

/// A suggested next step for a job, with the reason in a person's terms.
struct Advice {
    /// The path under `/jobs/{id}/` that does it.
    action: String,
    label: &'static str,
    why: String,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsView {
    /// Waiting on a decision: review, suspect, failed.
    attention: Vec<Job>,
    /// Downloading or importing.
    underway: Vec<Job>,
    /// Imported, or cancelled by someone: the newest few hundred.
    settled: Vec<Settled>,
    /// Older jobs exist beyond those shown.
    more: bool,
    /// Queue places by job id, for those waiting in a peer's queue.
    places: HashMap<String, u32>,
    /// Whether a Discogs release can be applied as well as a MusicBrainz one.
    discogs: bool,
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
    /// Where the download stands, in the terms that explain a wait: nothing
    /// yet, a place in the peer's queue, or how much has arrived.
    fn progress(&self, j: &Job) -> String {
        if let Some(host) = &j.host {
            return match (j.downloaded_bytes, j.total_bytes) {
                (0, _) => format!("waiting for {host}"),
                (got, 0) => format!("{} so far", human(got)),
                _ => format!(
                    "{} of {} ({}%)",
                    human(j.downloaded_bytes),
                    human(j.total_bytes),
                    self.pct(j)
                ),
            };
        }
        let peer = j.username.as_deref().unwrap_or("the peer");
        match self.places.get(j.id.as_str()) {
            Some(p) if j.downloaded_bytes == 0 => format!("place {p} in {peer}'s queue"),
            Some(p) => format!(
                "{} of {}, rest at place {p} in {peer}'s queue",
                human(j.downloaded_bytes),
                human(j.total_bytes)
            ),
            None if j.downloaded_bytes == 0 => "waiting for the peer".to_string(),
            None => format!(
                "{} of {} ({}%)",
                human(j.downloaded_bytes),
                human(j.total_bytes),
                self.pct(j)
            ),
        }
    }
    /// The signal that is true while a request from this job's card is in
    /// flight. Local (leading underscore), so it is never sent back.
    fn sig(&self, j: &Job) -> String {
        self.sig_of(j.id.as_str())
    }
    fn sig_of(&self, id: &str) -> String {
        format!("_busy_{}", id.replace('-', ""))
    }
    fn label(&self, status: &str) -> &'static str {
        status_label(status)
    }

    /// What was fetched, as the peer named it: the folder and the one above,
    /// which is usually the artist. The query that found it otherwise.
    fn heading(&self, j: &Job) -> String {
        let Some(folder) = j.folder.as_deref() else {
            return j.title.clone();
        };
        let parts: Vec<_> = folder
            .rsplit(['\\', '/'])
            .filter(|p| !p.is_empty())
            .take(2)
            .collect();
        match parts.as_slice() {
            [album, parent] => format!("{parent} / {album}"),
            [album] => album.to_string(),
            _ => j.title.clone(),
        }
    }

    /// The client-only signal that shows a job's files.
    fn open_sig(&self, j: &Job) -> String {
        format!("_files_{}", j.id.as_str().replace('-', ""))
    }

    fn file_progress(&self, f: &graphql::JobFile) -> String {
        if let Some(e) = &f.error {
            return format!("{} · {e}", f.state);
        }
        match f.state.as_str() {
            "completed" => human(f.size),
            _ if f.bytes == 0 => f.state.clone(),
            _ => format!(
                "{} of {} ({}%)",
                human(f.bytes),
                human(f.size),
                percent(f.bytes, f.size)
            ),
        }
    }

    /// The query that started the job, when the heading shows something else.
    fn asked<'a>(&self, j: &'a Job) -> Option<&'a str> {
        j.folder.as_ref().map(|_| j.title.as_str())
    }

    /// Why an import as-is would be refused, when it has been checked and
    /// would be.
    fn as_is_blocked<'a>(&self, j: &'a Job) -> Option<&'a str> {
        j.as_is_blocker.as_deref().filter(|b| !b.is_empty())
    }

    /// The one action most likely right, and why: the judgement a person
    /// would otherwise make from the evidence on the card.
    fn advice(&self, j: &Job) -> Option<Advice> {
        let next = |why: String| Advice {
            action: "next".into(),
            label: "Try another copy",
            why,
        };
        match j.status.as_str() {
            "review" => {
                let best = j.candidates.first();
                if let Some(c) = best
                    && c.missing == 0
                    && c.extra == 0
                    && c.distance <= 0.12
                {
                    return Some(Advice {
                        action: format!("resolve/{}", c.release_id),
                        label: "Use this release",
                        why: format!(
                            "{} — {} lines up track for track; only details such as the credit differ.",
                            c.artist, c.title
                        ),
                    });
                }
                if best.is_none() && self.as_is_blocked(j).is_none() {
                    return Some(Advice {
                        action: "as-is".into(),
                        label: "Import as-is",
                        why: "MusicBrainz doesn't have this release, and the files are properly tagged.".into(),
                    });
                }
                if j.alternates > 0 {
                    return Some(next(match best {
                        None => "MusicBrainz doesn't have it and these files aren't tagged well enough to file; another copy may be.".into(),
                        Some(c) if c.missing > 0 => format!("This copy is missing {} tracks.", c.missing),
                        Some(_) => "No release fits these files closely.".into(),
                    }));
                }
                best.map(|c| Advice {
                    action: format!("resolve/{}", c.release_id),
                    label: "Use the closest release",
                    why: "No other copy to try.".into(),
                })
            }
            "suspect" => {
                let flagged: Vec<_> = j
                    .analysis
                    .iter()
                    .filter(|t| {
                        !matches!(
                            t.verdict,
                            crate::analysis::Verdict::Lossless
                                | crate::analysis::Verdict::Uncertain
                        )
                    })
                    .collect();
                let padded = !flagged.is_empty()
                    && flagged.iter().all(|t| {
                        t.estimate
                            .as_deref()
                            .is_some_and(|e| e.contains("16-bit content"))
                    });
                if padded {
                    return Some(Advice {
                        action: "approve".into(),
                        label: "Import anyway",
                        why:
                            "Only padded: CD-quality audio in a 24-bit container. Nothing is lost."
                                .into(),
                    });
                }
                if j.alternates > 0 {
                    return Some(next(
                        "The audio may not be true lossless; another copy may be clean.".into(),
                    ));
                }
                Some(Advice {
                    action: "approve".into(),
                    label: "Import anyway",
                    why: "No other copy; the audio may not be true lossless.".into(),
                })
            }
            "failed" if j.alternates > 0 => Some(next("This copy couldn't be fetched.".into())),
            "failed" => Some(Advice {
                action: "retry".into(),
                label: "Retry",
                why: "No other copy was found.".into(),
            }),
            _ => None,
        }
    }

    /// Whether to show `action` among the other choices: it applies, and
    /// is not already the suggestion.
    fn offer(&self, j: &Job, action: &str) -> bool {
        let applies = match action {
            "approve" => j.status == "suspect",
            "next" => {
                matches!(j.status.as_str(), "review" | "suspect" | "failed") && j.alternates > 0
            }
            "as-is" => j.status == "review" && self.as_is_blocked(j).is_none(),
            "retry" => j.status == "failed",
            "match" => j.status == "review",
            "cancel" => j.status == "downloading",
            _ => false,
        };
        applies && self.advice(j).is_none_or(|a| a.action != action)
    }

    /// What the job needs from the person, in their terms; the reason
    /// underneath is the machine's.
    fn ask(&self, j: &Job) -> Option<&'static str> {
        match j.status.as_str() {
            "review" if j.candidates.is_empty() => Some(
                "MusicBrainz has nothing that fits these files. Import them as they are tagged, or try another copy.",
            ),
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

/// The newest `shown` jobs in hand, and the newest `shown` of those done
/// with.
async fn jobs_html(app: &App, shown: i64) -> String {
    let shown = shown.clamp(1, MOST);
    let (mut active, mut done) = crate::db::job_board(&app.db, shown + 1)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "could not list jobs"))
        .unwrap_or_default();
    let more = shown < MOST && (active.len() as i64 > shown || done.len() as i64 > shown);
    active.truncate(shown as usize);
    done.truncate(shown as usize);
    let jobs = graphql::job_views(app, active, true)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = ?e, "could not show jobs");
            Vec::new()
        });
    let places = jobs
        .iter()
        .filter_map(|j| {
            let id = uuid::Uuid::parse_str(j.id.as_str()).ok()?;
            Some((j.id.to_string(), app.jobs.place(id)?))
        })
        .collect();
    let mut view = JobsView {
        attention: Vec::new(),
        underway: Vec::new(),
        settled: done.into_iter().map(Settled::from).collect(),
        more,
        places,
        discogs: app.jobs.discogs(),
    };
    for j in jobs {
        match j.status.as_str() {
            "review" | "suspect" | "failed" => view.attention.push(j),
            _ => view.underway.push(j),
        }
    }
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "uploads.html")]
struct UploadsView {
    peers: Vec<Peer>,
    sending: usize,
    queued: usize,
    sent: usize,
    shared: usize,
    history: Vec<crate::db::UploadRow>,
    /// Older history exists beyond that shown.
    more: bool,
    /// Completed uploads today and this week: (files, bytes).
    today: (i64, i64),
    week: (i64, i64),
}

/// One person taking files from us.
struct Peer {
    username: String,
    files: Vec<Transfer>,
    done: usize,
    speed: u64,
}

impl UploadsView {
    fn bytes(&self, b: &i64) -> String {
        human((*b).max(0) as u64)
    }
    fn files(&self, n: &i64) -> String {
        if *n == 1 {
            "1 file".into()
        } else {
            format!("{n} files")
        }
    }
    /// Average speed over the time it was seen sending, when that is known.
    fn rate(&self, h: &crate::db::UploadRow) -> String {
        match h.seconds.filter(|s| *s >= 1.0) {
            Some(s) => format!("{}/s", human((h.bytes.max(0) as f64 / s) as u64)),
            None => String::new(),
        }
    }
    fn ago(&self, h: &crate::db::UploadRow) -> String {
        ago(h.finished_at)
    }
    fn leaf<'a>(&self, name: &'a str) -> &'a str {
        name.rsplit(['\\', '/']).next().unwrap_or(name)
    }
    fn pct(&self, t: &Transfer) -> u64 {
        percent(t.bytes, t.size)
    }
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
    fn label(&self, t: &Transfer) -> String {
        match (t.state.as_str(), t.place) {
            ("queued", Some(p)) => format!("queued #{p}"),
            ("queued", None) => "queued".into(),
            ("transferring", _) => "sending".into(),
            (s, _) => s.into(),
        }
    }
}

/// The Uploads tab's badge: files being sent or waiting to be.
fn up_count_html(app: &App) -> String {
    let n = app.session.engine().map_or(0, |e| {
        e.uploads()
            .iter()
            .filter(|t| !matches!(t.state, "completed" | "cancelled" | "failed"))
            .count()
    });
    let text = if n > 0 { n.to_string() } else { String::new() };
    format!(r#"<span id="up-count" class="text-ink empty:hidden">{text}</span>"#)
}

/// Who is taking what from us, grouped by person: those receiving now
/// first, then those waiting in the queue; then the newest `shown` of
/// those finished.
async fn uploads_html(app: &App, shown: i64) -> String {
    let mut peers: Vec<Peer> = Vec::new();
    let (mut sending, mut queued, mut sent, mut shared) = (0, 0, 0, 0);
    if let Some(e) = app.session.engine() {
        shared = e.share_counts().1;
        let mut by_user: std::collections::BTreeMap<String, Peer> = Default::default();
        for t in e.uploads() {
            let peer = by_user.entry(t.username.clone()).or_insert_with(|| Peer {
                username: t.username.clone(),
                files: Vec::new(),
                done: 0,
                speed: 0,
            });
            match t.state {
                "completed" => {
                    peer.done += 1;
                    sent += 1;
                }
                "cancelled" | "failed" => {}
                state => {
                    if state == "transferring" {
                        sending += 1;
                    } else {
                        queued += 1;
                    }
                    peer.speed += t.speed;
                    peer.files.push(Transfer::from(t));
                }
            }
        }
        peers = by_user
            .into_values()
            .filter(|p| !p.files.is_empty())
            .collect();
        peers.sort_by_key(|p| std::cmp::Reverse(p.speed));
    }
    let now = chrono::Utc::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc())
        .unwrap_or(now);
    let shown = shown.clamp(1, MOST);
    let mut history = crate::db::recent_uploads(&app.db, shown + 1)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "could not read upload history"))
        .unwrap_or_default();
    let more = shown < MOST && history.len() as i64 > shown;
    history.truncate(shown as usize);
    let today = crate::db::upload_totals(&app.db, midnight)
        .await
        .unwrap_or_default();
    let week = crate::db::upload_totals(&app.db, now - chrono::Duration::days(7))
        .await
        .unwrap_or_default();
    UploadsView {
        peers,
        sending,
        queued,
        sent,
        shared,
        history,
        more,
        today,
        week,
    }
    .render()
    .unwrap_or_default()
}

/// The stream's request: the page it serves, as `PAGES` names it, and the
/// page's signals as Datastar sends them.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct StreamQuery {
    page: String,
    datastar: String,
}

/// How many rows of the long lists the page shows, from the signals
/// Datastar sends with the stream's request.
#[derive(serde::Deserialize)]
#[serde(default)]
struct Shown {
    jobs: i64,
    history: i64,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            jobs: PAGE,
            history: PAGE,
        }
    }
}

/// Re-render the live parts of the page `page` names once a second, sending
/// each only when it changed: the status line and the nav's badges on every
/// page, and the jobs, uploads or library run on their own. The page
/// reopens the stream when the rows it shows change.
///
/// The stream ends itself after a while and the page reopens it on an
/// interval: a stream can die without either end noticing (a suspended
/// phone, a replaced pod), and reopening on a timer is what bounds how long
/// a page can sit showing stale state.
async fn stream(
    State(s): State<UiState>,
    Query(StreamQuery { page, datastar }): Query<StreamQuery>,
) -> impl IntoResponse {
    let app = s.app.clone();
    let shown: Shown = serde_json::from_str(&datastar).unwrap_or_default();
    let events = async_stream::stream! {
        // The library run is sent when it moves, and while it runs only
        // its head and the albums done since: it lists every album, which
        // in a large run is long.
        let mut run_sent = library::RunSent::default();
        let mut sent: [String; 4] = Default::default();
        for _ in 0..20 {
            let parts = [
                status_html(&app).await,
                match page.as_str() {
                    "albums" => jobs_html(&app, shown.jobs).await,
                    "uploads" => uploads_html(&app, shown.history).await,
                    _ => String::new(),
                },
                up_count_html(&app),
                chat::count_html(&app).await,
            ];
            for (html, last) in parts.into_iter().zip(&mut sent) {
                if !html.is_empty() && html != *last {
                    yield Ok(patch(&html));
                    *last = html;
                }
            }
            if page == "library" {
                for e in library::run_events(&s, &mut run_sent) {
                    yield Ok(e);
                }
            }
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

/// How many jobs the page shows, sent only by the page that lists them.
#[derive(serde::Deserialize)]
struct GrabQuery {
    jobs: Option<i64>,
}

/// A search's view, all of it in the address.
#[derive(serde::Deserialize, Default)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    /// Any value means lossless, as the page reads it.
    #[serde(default)]
    lossless: String,
    /// How many folders are drawn.
    #[serde(default)]
    shown: String,
}

/// Folders shown at first, and added by each "Show more", in search results.
/// A generic query is answered with thousands, and a page drawing them all
/// every second stops responding.
const FOUND: usize = 100;

/// How long the web UI's searches listen for peers: as long as a grab, so
/// either stands in for the other.
const LISTEN: u64 = 10;

#[derive(Template)]
#[template(path = "results.html")]
struct ResultsView {
    query: String,
    folders: Vec<Folder>,
    /// Folders found beyond those shown.
    more: usize,
    /// The address that shows another page of them.
    next: String,
    searching: bool,
    responses: usize,
}

impl ResultsView {
    fn key(&self, f: &Folder) -> String {
        URL_SAFE_NO_PAD.encode(f.remote_path.as_bytes())
    }
    /// A file as the download form sends it: its path, then its size.
    fn file_key(&self, x: &crate::folders::File) -> String {
        format!("{}.{}", URL_SAFE_NO_PAD.encode(x.remote.as_bytes()), x.size)
    }
    /// Stable across regroups, so an open preview stays with its folder.
    fn id(&self, f: &Folder) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (&f.username, f.remote_path.as_bytes()).hash(&mut h);
        h.finish()
    }
    fn crumbs(&self, f: &Folder) -> Vec<browse::Child> {
        browse::crumbs(f.remote_path.as_bytes())
    }
    /// A file's path below the result's folder, which keeps a disc
    /// subfolder's name when discs are grouped into one result.
    fn within(&self, f: &Folder, file: &crate::folders::File) -> String {
        let full = file.remote.as_bytes();
        let rest = full
            .strip_prefix(f.remote_path.as_bytes())
            .and_then(|r| r.strip_prefix(b"\\"))
            .unwrap_or(full);
        RawStr(rest.to_vec().into())
            .to_string_lossy()
            .replace('\\', "/")
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

/// The results for the address's query, regrouped as peers answer, at most
/// once a second, the first `shown` of them. The search is shared with any of the same words from the
/// last ten minutes, so reopening or reloading the page, or showing more,
/// sends nothing to the network.
async fn search(State(s): State<UiState>, Query(view): Query<SearchQuery>) -> Response {
    let Some(engine) = s.app.session.engine().cloned() else {
        return one(
            r#"<div id="results" class="text-meta text-muted">Sign in to Soulseek first.</div>"#
                .into(),
        );
    };
    let query = view.q.trim().to_string();
    if query.is_empty() {
        return one(r#"<div id="results"></div>"#.into());
    }
    let filter = Filter {
        lossless: !view.lossless.is_empty(),
        ..Default::default()
    };
    let lossless = if filter.lossless { "true" } else { "" };
    let found = view
        .shown
        .parse()
        .map_or(FOUND, |n: usize| n.clamp(1, MOST as usize));
    let next = place_url(
        "/search",
        &[
            ("q", &query),
            ("lossless", lossless),
            ("shown", &(found + FOUND).to_string()),
        ],
    );
    let events = async_stream::stream! {
        let mut heard = graphql::listen(&engine, &query, LISTEN);
        loop {
            let crate::graphql::Heard { responses, done, error } = heard.borrow_and_update().clone();
            if let Some(e) = error {
                yield Ok(patch(&format!(
                    r#"<div id="results" class="text-meta text-muted">{}</div>"#,
                    escape(&e)
                )));
                break;
            }
            let mut folders = crate::folders::relevant(crate::folders::group(&responses, &filter), &query);
            let more = folders.len().saturating_sub(found);
            folders.truncate(found);
            let view = ResultsView { query: query.clone(), folders, more, next: next.clone(), searching: !done, responses: responses.len() };
            yield Ok(patch(&view.render().unwrap_or_default()));
            if done || heard.changed().await.is_err() {
                break;
            }
        }
    };
    sse(events).into_response()
}

async fn grab(
    State(s): State<UiState>,
    Query(shown): Query<GrabQuery>,
    axum::Form(form): axum::Form<SearchForm>,
) -> Response {
    let filter = form.lossless.then(|| Filter {
        lossless: true,
        ..Default::default()
    });
    match graphql::grab(&s.app, form.q.trim(), LISTEN, filter, false).await {
        // Albums shows the new card; Search has no job list, so it says so.
        Ok(_) => match shown.jobs {
            Some(shown) => done(&s.app, shown).await,
            None => one(flash_ok(&format!("Grabbing {}", form.q.trim()))),
        },
        Err(e) => one(flash_err(&e.message)),
    }
}

#[derive(serde::Deserialize)]
struct UrlForm {
    url: String,
    #[serde(default = "page", deserialize_with = "shown_field")]
    jobs: i64,
}

async fn import_url(State(s): State<UiState>, axum::Form(form): axum::Form<UrlForm>) -> Response {
    match s.app.jobs.from_url(&form.url).await {
        Ok(_) => done(&s.app, form.jobs).await,
        Err(e) => failed(&e),
    }
}

/// A folder from the search results. The files the results listed come
/// with it, to fetch by name when the peer will not list the folder.
async fn download(State(s): State<UiState>, body: Bytes) -> Response {
    let fields = fields(&body);
    let (Some(username), Some(Ok(raw))) = (
        field(&fields, "username"),
        field(&fields, "key").map(|k| URL_SAFE_NO_PAD.decode(k)),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let files = fields
        .iter()
        .filter(|(k, _)| k == "file")
        .filter_map(|(_, v)| {
            let (path, size) = v.rsplit_once('.')?;
            let remote = RawStr(URL_SAFE_NO_PAD.decode(path).ok()?.into());
            Some(crate::folders::File {
                name: remote.to_string_lossy(),
                extension: String::new(),
                size: size.parse().ok()?,
                remote,
                bitrate: None,
                duration: None,
                sample_rate: None,
                bit_depth: None,
            })
        })
        .collect();
    let remote = RawStr(raw.into());
    let folder = Folder {
        username: username.to_string(),
        path: remote.to_string_lossy(),
        remote_path: remote,
        files,
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
    let title = field(&fields, "title").map(str::to_string);
    match s.app.jobs.from_folder(&folder, title, Vec::new()).await {
        Ok(job) => one(flash_ok(&format!("Downloading {}", job.title))),
        Err(e) => failed(&e),
    }
}

async fn job_action(
    State(s): State<UiState>,
    Path((id, action)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    let jobs = &s.app.jobs;
    let result = match action.as_str() {
        "retry" => jobs.retry(id).await,
        "cancel" => jobs.cancel(id).await,
        "remove" => jobs.remove(id).await,
        "approve" => jobs.import_soon(id, true, None).await,
        "next" => jobs.next_source(id, crate::jobs::cause::REQUESTED).await,
        // Waited for, since it is quick without MusicBrainz, so a refusal
        // is reported to the person who asked rather than looking like a
        // tap that did nothing.
        "as-is" => match jobs.import_as_is(id, sift::Edits::default()).await {
            Ok(()) => match crate::db::job(&s.app.db, id).await {
                Ok(Some(j)) if j.status == "review" => Err(anyhow::anyhow!(
                    "Couldn't import as-is: {}",
                    j.error.unwrap_or_default()
                )),
                _ => Ok(()),
            },
            Err(e) => Err(e),
        },
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match result {
        Ok(()) => done(&s.app, jobs_shown(&body)).await,
        Err(e) => failed(&e),
    }
}

/// The job list as it now stands, as many rows as the page shows, and any
/// earlier error cleared: the change on the card is the confirmation, and
/// the button stays busy until it is drawn.
async fn done(app: &App, shown: i64) -> Response {
    one(format!(
        "{}\n<div id=\"flash\"></div>",
        jobs_html(app, shown).await
    ))
}

/// How many jobs the page shows, from the signals a JSON request carries.
fn jobs_shown(body: &[u8]) -> i64 {
    serde_json::from_slice::<Shown>(body)
        .unwrap_or_default()
        .jobs
}

fn failed(e: &anyhow::Error) -> Response {
    one(flash_err(&format!("{e:#}")))
}

async fn resolve(
    State(s): State<UiState>,
    Path((id, release)): Path<(Uuid, String)>,
    body: Bytes,
) -> Response {
    let Some(release) = crate::library::release_id(&release) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => done(&s.app, jobs_shown(&body)).await,
        Err(e) => failed(&e),
    }
}

#[derive(serde::Deserialize)]
struct ReleaseForm {
    release: String,
    #[serde(default = "page", deserialize_with = "shown_field")]
    jobs: i64,
}

/// A release named by hand, for when MusicBrainz or Discogs has it but the
/// matcher did not offer it. Takes the bare id or a pasted release link.
async fn resolve_pasted(
    State(s): State<UiState>,
    Path(id): Path<Uuid>,
    axum::Form(form): axum::Form<ReleaseForm>,
) -> Response {
    let Some(release) = crate::library::release_id(&form.release) else {
        return one(flash_err(
            "That is not a MusicBrainz or Discogs release id or link.",
        ));
    };
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => done(&s.app, form.jobs).await,
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

/// A job's files and the tags they carry, read when its card asks: the
/// evidence for a choice, which the card itself does not hold.
#[derive(Template)]
#[template(path = "job_files.html")]
struct JobFiles {
    id: Uuid,
    error: Option<String>,
    /// Each album the tags name, with a MusicBrainz search for it.
    albums: Vec<(String, String)>,
    tracks: Vec<sift::meta::Track>,
}

impl JobFiles {
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

async fn job_files(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
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
    let html = JobFiles {
        id,
        error,
        albums,
        tracks,
    }
    .render()
    .unwrap_or_default();
    many(vec![insert(&format!("#files-{id}"), "replace", &html)])
}

/// A job's files against one release, track by track, as beets shows a
/// match before it is taken.
#[derive(Template)]
#[template(path = "job_compare.html")]
struct JobCompare {
    id: String,
    error: Option<String>,
    cmp: Option<sift::Comparison>,
}

impl JobCompare {
    fn dist(&self, d: &f64) -> String {
        format!("{d:.2}")
    }
    fn delta(&self, d: &f64) -> String {
        format!("{d:+.0}s")
    }
}

/// The element a job's comparison with a release fills. Discogs ids hold a
/// colon, which a selector would read as a pseudo-class.
pub(crate) fn compare_id(job: impl std::fmt::Display, release: &str) -> String {
    format!("{job}-{}", release.replace(':', "-"))
}

async fn job_compare(
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
    let id = compare_id(id, &release);
    let html = JobCompare {
        id: id.clone(),
        error,
        cmp,
    }
    .render()
    .unwrap_or_default();
    many(vec![insert(&format!("#cmp-{id}"), "replace", &html)])
}

#[derive(serde::Deserialize)]
struct UserFileForm {
    username: String,
    filename: Option<String>,
    #[serde(default = "page", deserialize_with = "shown_field")]
    history: i64,
}

async fn cancel_upload(
    State(s): State<UiState>,
    axum::Form(f): axum::Form<UserFileForm>,
) -> Response {
    if let (Some(e), Some(name)) = (s.app.session.engine(), f.filename) {
        e.cancel_upload(&f.username, &RawStr::from(name));
    }
    one(uploads_html(&s.app, f.history).await)
}

async fn ban(State(s): State<UiState>, axum::Form(f): axum::Form<UserFileForm>) -> Response {
    match graphql::set_ban(&s.app, &f.username, true).await {
        Ok(_) => one(flash_ok(&format!("Banned {}", f.username))),
        Err(e) => one(flash_err(&e.message)),
    }
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
        Err(e) => failed(&e),
    }
}

async fn reconnect(State(s): State<UiState>) -> Response {
    if let Some(e) = s.app.session.engine() {
        e.reconnect();
    }
    one(status_html(&s.app).await)
}

fn escape(s: &str) -> String {
    askama::filters::escape(s, askama::filters::Html).map_or_else(|e| match e {}, |s| s.to_string())
}
