//! The web UI: server-rendered HTML patched over SSE by Datastar.
//!
//! One page. A stream pushes the status line and the open tab's jobs or
//! uploads once a second, each only when it changed; searching streams its own results as peers answer. Every
//! route but sign-in, the probe and the static assets sits behind the
//! session layer, which wraps the router whole so a route added later is
//! protected by construction.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
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
mod wishlist;

#[derive(Clone)]
pub struct UiState {
    pub app: Arc<App>,
    pub config: Arc<Config>,
    pub sessions: Sessions,
    pub http: reqwest::Client,
    /// The library action running in the background, or the last one.
    run: Arc<parking_lot::Mutex<library::LibraryRun>>,
    browsed: browse::Cache,
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
        browsed: Default::default(),
    };
    let protected = Router::new()
        .route("/", get(page))
        // No content of its own to fetch.
        .route("/uploads", get(page))
        .route("/stream", get(stream))
        .route("/search", post(search))
        .route("/grab", post(grab))
        .route("/download", post(download))
        .route("/jobs/{id}/{action}", post(job_action))
        .route("/jobs/{id}/resolve/{release}", post(resolve))
        .route("/jobs/{id}/release", post(resolve_pasted))
        .route("/jobs/{id}/spectrogram/{n}", get(spectrogram))
        .route("/uploads/cancel", post(cancel_upload))
        .route("/ban", post(ban))
        .route("/account", post(account))
        .route("/reconnect", post(reconnect))
        .merge(wishlist::routes())
        .merge(library::routes())
        .merge(browse::routes())
        .merge(chat::routes())
        .merge(admin::routes())
        .layer(axum::middleware::from_fn(require_datastar_on_post))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            tab_pages,
        ))
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
        r#"<div id="flash" class="flash ok" role="status" data-on:click="el.className = ''">{}</div>"#,
        askama_escape(msg)
    )
}

fn flash_err(msg: &str) -> String {
    format!(
        r#"<div id="flash" class="flash error" role="alert" data-on:click="el.className = ''">{}</div>"#,
        askama_escape(msg)
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

#[derive(Template)]
#[template(path = "page.html")]
struct Page {
    status: String,
    jobs: String,
    uploads: String,
    up_count: String,
    chat_count: String,
    run: String,
    version: &'static str,
    /// The tab the address names, open on arrival.
    tab: &'static str,
    /// The page's starting signals, from the address: the tab, where in it,
    /// and a search to fill in.
    signals: String,
    /// The address carries an album search; run it again on arrival.
    search: bool,
}

/// Tabs with an address of their own, `/<tab>`. The albums tab is `/`.
const TABS: &[&str] = &[
    "uploads", "wishlist", "library", "browse", "chat", "bans", "settings", "triage",
];

/// A browser asking for a tab's address gets the page open on that tab. The
/// same address fetched by the page itself (`Datastar-Request`) is the
/// tab's content, and goes on to its own handler.
async fn tab_pages(State(s): State<UiState>, request: Request, next: Next) -> Response {
    if request.method() == http::Method::GET
        && !request.headers().contains_key("datastar-request")
        && let Some(tab) = TABS
            .iter()
            .find(|t| request.uri().path().strip_prefix('/') == Some(**t))
    {
        return match render_page(&s, tab, request.uri()).await {
            Ok(html) => html.into_response(),
            Err(e) => e.into_response(),
        };
    }
    next.run(request).await
}

async fn page(
    State(s): State<UiState>,
    uri: http::Uri,
) -> Result<Html<String>, crate::error::AppError> {
    let tab = TABS
        .iter()
        .find(|t| uri.path().strip_prefix('/') == Some(**t))
        .copied()
        .unwrap_or("albums");
    render_page(&s, tab, &uri).await
}

/// A user's shares at a folder (the top when `key` is empty).
fn browse_href(username: &str, key: &str) -> String {
    place_url("/browse", &[("username", username), ("key", key)])
}

/// A conversation: `kind` is `room` or `pm`.
fn chat_href(kind: &str, name: &str) -> String {
    place_url("/chat", &[("kind", kind), ("name", name)])
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

/// Where `tab` now is, for the address bar (`_url`) and for coming back to
/// the tab (`_at`). Every action that moves within a tab reports it, so a
/// reload, a shared link or back and forward return to the same place: the
/// address is the one the page fetches to draw it.
fn place(tab: &str, url: &str) -> Event {
    signals(&serde_json::json!({ "_url": url, "_at": { tab: url } }).to_string())
}

async fn render_page(
    s: &UiState,
    tab: &'static str,
    uri: &http::Uri,
) -> Result<Html<String>, crate::error::AppError> {
    let url = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    let params: Vec<(String, String)> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let param = |k: &str| field(&params, k).unwrap_or_default().to_string();
    let q = if tab == "albums" {
        param("q")
    } else {
        String::new()
    };
    let signals = serde_json::json!({
        "q": q,
        "lossless": q.is_empty() || !param("lossless").is_empty(),
        "busy": false,
        "tab": tab,
        "settled": PAGE,
        "history": PAGE,
        "_url": url,
        "_at": { tab: url },
        "_libq": if tab == "library" { param("q") } else { String::new() },
        "_libsince": if tab == "library" { param("since") } else { String::new() },
        "_libfrom": if tab == "library" { param("from") } else { String::new() },
        "_libshow": if tab == "library" { param("show") } else { String::new() },
        "_libalbum": if tab == "library" { param("album") } else { String::new() },
    })
    .to_string();
    // Hidden tabs start empty: the stream fills the open one, and drawing
    // the others first held up a page opened on a tab of its own.
    let page = Page {
        status: status_html(&s.app).await,
        jobs: if tab == "albums" {
            jobs_html(&s.app, PAGE).await
        } else {
            r#"<div id="jobs" class="jobs"></div>"#.into()
        },
        uploads: if tab == "uploads" {
            uploads_html(&s.app, PAGE).await
        } else {
            r#"<div id="uploads" class="uploads"></div>"#.into()
        },
        up_count: up_count_html(&s.app),
        chat_count: chat::count_html(&s.app).await,
        run: library::run_html(s),
        version: env!("CARGO_PKG_VERSION"),
        tab,
        search: !q.is_empty(),
        signals,
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
/// grow without end: settled jobs and upload history. Rendering every row
/// made the page heavy enough to crash phone browsers.
const PAGE: i64 = 500;

/// A job that landed or was cancelled, as its one line.
struct Settled {
    id: String,
    /// What landed, as filed ("Artist — (Year) Album"), which is the
    /// confirmation a person wants; the query that asked for it otherwise.
    title: String,
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
        Self {
            id: j.id.to_string(),
            title,
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
    /// Older settled jobs exist beyond those shown.
    more: bool,
    /// Queue places by job id, for those waiting in a peer's queue.
    places: HashMap<String, u32>,
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

/// Every job in hand, and the newest `settled` of those done with.
async fn jobs_html(app: &App, settled: i64) -> String {
    let (active, mut done) = crate::db::job_board(&app.db, settled + 1)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "could not list jobs"))
        .unwrap_or_default();
    let more = done.len() as i64 > settled;
    done.truncate(settled.max(0) as usize);
    let jobs = graphql::job_views(app, active).await.unwrap_or_else(|e| {
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
    fn short<'a>(&self, name: &'a str) -> &'a str {
        name.rsplit('\\').next().unwrap_or(name)
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
    format!(r#"<span id="up-count" class="count">{text}</span>"#)
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
    let mut history = crate::db::recent_uploads(&app.db, shown + 1)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "could not read upload history"))
        .unwrap_or_default();
    let more = history.len() as i64 > shown;
    history.truncate(shown.max(0) as usize);
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

/// What the page has open, from the signals Datastar sends with the
/// stream's request.
#[derive(serde::Deserialize)]
#[serde(default)]
struct Shown {
    tab: String,
    settled: i64,
    history: i64,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            tab: "albums".into(),
            settled: PAGE,
            history: PAGE,
        }
    }
}

/// Re-render the live parts once a second, sending each only when it
/// changed, and the jobs and uploads only while their tab is open: morphing
/// thousands of unchanged rows every second is what made the page crawl.
/// The page reopens the stream when the tab or the rows shown change.
///
/// The stream ends itself after a while and the page reopens it on an
/// interval: a stream can die without either end noticing (a suspended
/// phone, a replaced pod), and reopening on a timer is what bounds how long
/// a page can sit showing stale state.
async fn stream(
    State(s): State<UiState>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let app = s.app.clone();
    let shown: Shown = q
        .get("datastar")
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let events = async_stream::stream! {
        // The library run is sent when it moves, and while it runs only
        // its head and the albums done since: it lists every album, which
        // in a large run is long.
        let mut run_sent = library::RunSent::default();
        let mut sent: [String; 5] = Default::default();
        for _ in 0..20 {
            let parts = [
                status_html(&app).await,
                if shown.tab == "albums" {
                    jobs_html(&app, shown.settled).await
                } else {
                    String::new()
                },
                if shown.tab == "uploads" {
                    uploads_html(&app, shown.history).await
                } else {
                    String::new()
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
            for e in library::run_events(&s, &mut run_sent) {
                yield Ok(e);
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
    let here = place(
        "albums",
        &place_url(
            "/",
            &[
                ("q", &query),
                ("lossless", if form.lossless { "1" } else { "" }),
            ],
        ),
    );
    let rx = match engine.pace().await {
        Ok(()) => engine.search(&query),
        Err(e) => Err(e),
    };
    let events = async_stream::stream! {
        yield Ok(here);
        let mut rx = match rx {
            Ok(rx) => rx,
            Err(e) => {
                yield Ok(patch(&format!(
                    r#"<div id="results" class="note">{}</div>"#,
                    askama_escape(&e.to_string())
                )));
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        let mut responses = Vec::new();
        loop {
            let tick = tokio::time::Instant::now() + Duration::from_millis(1000);
            while let Ok(Some(r)) = tokio::time::timeout_at(tick.min(deadline), rx.recv()).await {
                responses.push(r);
            }
            let done = tokio::time::Instant::now() >= deadline;
            let folders = crate::folders::relevant(crate::folders::group(&responses, &filter), &query);
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
    match graphql::grab(&s.app, form.q.trim(), 10, filter, false).await {
        Ok(_) => done(),
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
        Ok(job) => one(flash_ok(&format!("Downloading {}", job.title))),
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
        Ok(()) => done(),
        Err(e) => failed(&e),
    }
}

/// Any earlier error cleared: the change on the card, which the stream
/// draws within a second, is the confirmation. The stream alone draws the
/// job list, so it keeps the rows the page asked to see.
fn done() -> Response {
    one("<div id=\"flash\"></div>".into())
}

fn failed(e: &anyhow::Error) -> Response {
    one(flash_err(&format!("{e:#}")))
}

async fn resolve(State(s): State<UiState>, Path((id, release)): Path<(Uuid, String)>) -> Response {
    if Uuid::parse_str(&release).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match s.app.jobs.import_soon(id, false, Some(release)).await {
        Ok(()) => done(),
        Err(e) => failed(&e),
    }
}

#[derive(serde::Deserialize)]
struct ReleaseForm {
    release: String,
}

/// A release named by hand, for when MusicBrainz has it but the matcher
/// did not offer it. Takes the bare ID or a pasted release URL.
async fn resolve_pasted(
    State(s): State<UiState>,
    Path(id): Path<Uuid>,
    axum::Form(form): axum::Form<ReleaseForm>,
) -> Response {
    let Some(release) = form
        .release
        .split(['/', '?', '#'])
        .find_map(|p| Uuid::parse_str(p.trim()).ok())
    else {
        return one(flash_err("That is not a MusicBrainz release ID or URL."));
    };
    match s
        .app
        .jobs
        .import_soon(id, false, Some(release.to_string()))
        .await
    {
        Ok(()) => done(),
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
    one("<div id=\"flash\"></div>".into())
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
