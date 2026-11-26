//! The web UI: server-rendered HTML patched over SSE by Datastar.
//!
//! Each tab in the nav is a module under [`page`], drawn by its own template
//! under `templates/<tab>/` around the shared layout ([`layout`]). A page
//! names a few regions by id, and Datastar morphs a region whole when the
//! server sends it again: a handler answering an action, or the page's live
//! stream ([`stream`]), which sends a region only when it changed. Markup
//! comes from templates and the kit's components alone, so every value in it
//! is escaped exactly once, where a template writes it.
//!
//! Every route but sign-in, the probe and the static assets sits behind the
//! session layer, which wraps the router whole so a route added later is
//! protected by construction.

use std::sync::Arc;

use askama::Template;
use askama_datastar::{Asset, Csp, ElementPatchMode, Html, PatchSignals, SecurityHeaders};
use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::response::sse::Event;
use axum::routing::get;
use koan_kit_web as kit;

use crate::App;
use crate::auth::session::Sessions;
use crate::config::Config;

#[cfg(all(test, unix))]
mod cost;
#[cfg(test)]
mod escaping;
#[cfg(test)]
mod fixture;
mod layout;
mod page;
mod peer;
mod stream;
#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct UiState {
    pub app: Arc<App>,
    pub config: Arc<Config>,
    pub sessions: Sessions,
    pub http: reqwest::Client,
    library: page::library::Runs,
    /// Album art previewed and not yet applied.
    art: page::library::ArtStash,
    browsed: page::browse::Cache,
    profiles: page::profile::Cache,
}

/// Every route the UI answers, in one place: the pages, each page's live
/// stream, and the actions the pages post to. A page is drawn by
/// [`page::serve`] and a stream run by [`stream::serve`], each from the
/// module named after its tab; an action is a handler in its page's module.
pub fn router(app: Arc<App>) -> Router {
    let state = UiState::new(app);
    page::library::resume(&state);
    routes(state)
}

impl UiState {
    fn new(app: Arc<App>) -> Self {
        Self {
            config: app.cfg.clone(),
            sessions: Sessions::new(app.db.clone()),
            app,
            http: reqwest::Client::new(),
            library: page::library::Runs::new(),
            art: Default::default(),
            browsed: Default::default(),
            profiles: Default::default(),
        }
    }
}

fn routes(state: UiState) -> Router {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;
    use page::{
        activity::Activity, bans, bans::Bans, browse, browse::Browse, chat, chat::Chat, downloads,
        downloads::Downloads, downloads::job::Job, library, library::Library, profile,
        profile::ProfilePage, search, search::Search, settings, settings::Settings, triage::Triage,
        uploads, uploads::Uploads, wishlist, wishlist::Wishlist,
    };
    use stream::{Quiet, tab};

    let pages = Router::new()
        .route("/", get(page::serve::<Downloads>))
        .route("/jobs/{id}", get(page::serve::<Job>))
        .route("/search", get(page::serve::<Search>))
        .route("/uploads", get(page::serve::<Uploads>))
        .route("/activity", get(page::serve::<Activity>))
        .route("/wishlist", get(page::serve::<Wishlist>))
        .route("/library", get(page::serve::<Library>))
        .route("/browse", get(page::serve::<Browse>))
        .route("/chat", get(page::serve::<Chat>))
        .route(
            "/profile",
            get(page::serve::<ProfilePage>)
                .post(profile::save)
                .layer(DefaultBodyLimit::max(profile::MAX_UPLOAD)),
        )
        .route("/bans", get(page::serve::<Bans>))
        .route("/settings", get(page::serve::<Settings>))
        .route("/triage", get(page::serve::<Triage>));
    let streams = Router::new()
        .route(
            "/stream/downloads",
            get(stream::serve::<stream::downloads::Downloads>),
        )
        .route("/stream/job", get(stream::serve::<stream::job::Job>))
        .route(
            "/stream/search",
            get(stream::serve::<stream::search::Search>),
        )
        .route(
            "/stream/uploads",
            get(stream::serve::<stream::uploads::Uploads>),
        )
        .route(
            "/stream/activity",
            get(stream::serve::<stream::activity::Activity>),
        )
        .route(
            "/stream/wishlist",
            get(stream::serve::<stream::wishlist::Wishlist>),
        )
        .route(
            "/stream/library",
            get(stream::serve::<stream::library::Library>),
        )
        .route(
            "/stream/browse",
            get(stream::serve::<stream::browse::Browse>),
        )
        .route("/stream/chat", get(stream::serve::<stream::chat::Chat>))
        .route(
            "/stream/profile",
            get(stream::serve::<stream::profile::Profile>),
        )
        .route(
            "/stream/bans",
            get(stream::serve::<Quiet<{ tab(page::Tab::Bans) }>>),
        )
        .route(
            "/stream/settings",
            get(stream::serve::<Quiet<{ tab(page::Tab::Settings) }>>),
        )
        .route(
            "/stream/triage",
            get(stream::serve::<Quiet<{ tab(page::Tab::Triage) }>>),
        );
    let actions = Router::new()
        // The status line and every page's peers.
        .route("/account", post(layout::account))
        .route("/reconnect", post(layout::reconnect))
        .route("/peer/ban", post(peer::ban))
        .route("/peer/unban", post(peer::unban))
        // Search
        .route("/download", post(search::download))
        .route("/search/tabs/{id}/pin", post(search::pin))
        .route("/search/tabs/{id}/close", post(search::close))
        .route("/search/tabs/{id}/again", post(search::again))
        .route("/search/tabs/close-unpinned", post(search::close_unpinned))
        // Downloads
        .route("/import-url", post(downloads::import_url))
        .route("/jobs/{id}/{action}", post(downloads::act))
        .route("/jobs/{id}/resolve/{release}", post(downloads::resolve))
        .route("/jobs/{id}/release", post(downloads::resolve_pasted))
        .route("/jobs/{id}/spectrogram/{n}", get(downloads::spectrogram))
        .route("/jobs/{id}/sources", get(downloads::sources))
        .route("/jobs/{id}/files", get(downloads::files))
        .route(
            "/jobs/{id}/pick",
            get(downloads::pick).post(downloads::pick_files),
        )
        .route("/jobs/{id}/compare/{release}", get(downloads::compare))
        // Uploads
        .route("/uploads/cancel", post(uploads::cancel))
        // Wishlist
        .route("/wishlist/add", post(wishlist::add))
        .route("/wishlist/{id}/remove", post(wishlist::remove))
        // Library
        .route("/library/more", post(library::more))
        .route("/library/album", get(library::album))
        .route("/library/album/releases", get(library::releases))
        .route("/library/album/edit", get(library::edit_form))
        .route("/library/album/cover", get(library::cover))
        .route("/library/dismiss", get(library::dismiss))
        .route(
            "/library/art/preview",
            post(library::art_preview)
                .layer(DefaultBodyLimit::max(crate::art::MAX_BYTES + (1 << 20))),
        )
        .route("/library/art/pending/{token}", get(library::pending_art))
        .route("/library/edit", post(library::batch_form))
        .route("/library/preview/{action}", post(library::plan::preview))
        .route("/library/apply/{action}", post(library::run::apply))
        // Browse
        .route("/browse/files", post(browse::download_files))
        .route("/browse/tree", post(browse::download_tree))
        // Chat
        .route("/chat/rooms", get(chat::rooms))
        .route("/chat/open", post(chat::open))
        .route("/chat/send", post(chat::send))
        .route("/chat/join", post(chat::join))
        .route("/chat/leave", post(chat::leave))
        .route("/chat/buddy/add", post(chat::add_buddy))
        .route("/chat/buddy/remove", post(chat::remove_buddy))
        // Profile
        .route("/profile/picture", get(profile::picture))
        // Bans
        .route("/bans/add", post(bans::ban))
        .route("/bans/remove", post(bans::unban))
        // Settings
        .route("/settings/slots", post(settings::slots))
        .route("/settings/limits", post(settings::limits))
        .route("/settings/rescan", post(settings::rescan))
        .route("/settings/interest", post(settings::interest));
    let protected = pages
        .merge(streams)
        .merge(actions)
        .layer(axum::middleware::from_fn(
            askama_datastar::require_datastar_on_post,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::extract::require_session,
        ));
    Router::new()
        .merge(protected)
        .merge(crate::auth::routes::router(state.sessions.clone()))
        .route("/healthz", get(|| async { "ok" }))
        .route("/assets/datastar.js", get(askama_datastar::script::serve))
        .route("/assets/live.js", get(LIVE_JS.serve()))
        .route("/assets/app.css", get(APP_CSS.serve()))
        .route("/assets/geist-mono.woff2", get(kit::font::serve))
        // Public like the other assets: a browser asks for the icon before anyone signs in.
        .route("/assets/icon-32.png", get(ICON_32.serve()))
        .route("/assets/icon-192.png", get(ICON_192.serve()))
        .route("/assets/apple-touch-icon.png", get(TOUCH_ICON.serve()))
        .layer(SecurityHeaders::new(&Csp::new().datastar()))
        .layer(axum::middleware::from_fn(compress_events))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state)
}

/// Event streams compressed as they go: tower-http leaves them alone,
/// since its encoders hold output back until a block fills and a live
/// stream's updates would wait. Here brotli is flushed after every event,
/// so each reaches the browser at once, and the window it keeps across the
/// stream makes an update that mostly repeats the last one cheap: a search
/// redrawn each second, or the downloads list.
async fn compress_events(request: Request, next: Next) -> Response {
    use std::io::Write as _;
    let br = request
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|e| e.split(';').next().is_some_and(|e| e.trim() == "br"))
        });
    let response = next.run(request).await;
    let events = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    if !br || !events {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        http::header::CONTENT_ENCODING,
        http::HeaderValue::from_static("br"),
    );
    parts.headers.append(
        http::header::VARY,
        http::HeaderValue::from_static("accept-encoding"),
    );
    parts.headers.remove(http::header::CONTENT_LENGTH);
    let mut data = body.into_data_stream();
    let stream = async_stream::stream! {
        // Quality 5 and a 4 MiB window: cheap enough for every event.
        let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        while let Some(chunk) = futures_util::StreamExt::next(&mut data).await {
            let Ok(chunk) = chunk else { break };
            if encoder.write_all(&chunk).and_then(|()| encoder.flush()).is_err() {
                break;
            }
            let out = std::mem::take(encoder.get_mut());
            if !out.is_empty() {
                yield Ok::<_, std::io::Error>(Bytes::from(out));
            }
        }
        yield Ok(Bytes::from(encoder.into_inner()));
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

static APP_CSS: Asset = Asset::new(
    include_bytes!("../assets/app.css"),
    "text/css; charset=utf-8",
);
static LIVE_JS: Asset = Asset::new(
    include_bytes!("../assets/live.js"),
    "text/javascript; charset=utf-8",
);
static ICON_32: Asset = Asset::new(include_bytes!("../assets/icon-32.png"), "image/png");
static ICON_192: Asset = Asset::new(include_bytes!("../assets/icon-192.png"), "image/png");
static TOUCH_ICON: Asset = Asset::new(
    include_bytes!("../assets/apple-touch-icon.png"),
    "image/png",
);

/// A short hash of `bytes` (FNV-1a) for an address that changes with them.
fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// A view rendered as markup, which another template embeds as it is: everything in it was escaped as the view was
/// rendered. A view that fails to render (a `Display` impl erring) is logged and drawn as nothing.
trait Draw: Template + Sized {
    fn draw(&self) -> Html {
        Html::render(self).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not render a view");
            Html::new()
        })
    }
}

impl<T: Template> Draw for T {}

/// The regions in `html` sent as a patch, each morphed into the page by its id.
fn patch(html: &Html) -> Event {
    html.patch().into()
}

/// One patch as the whole answer.
fn one(html: Html) -> Response {
    askama_datastar::events([patch(&html)])
}

/// Elements added inside the one `selector` names, at its end or start.
fn insert(selector: &str, mode: ElementPatchMode, html: &Html) -> Event {
    html.patch().selector(selector).mode(mode).into()
}

/// Signals set from the server, as JSON: how a form is cleared once what it sent has been accepted.
fn signals(json: serde_json::Value) -> Event {
    PatchSignals::new(json.to_string()).into()
}

/// An error, as the flash over the page.
fn failed(e: &anyhow::Error) -> Response {
    one(kit::Flash::bad(format!("{e:#}")).html())
}

/// A form body whose fields may repeat (checkboxes, several hidden paths),
/// which `axum::Form` does not collect.
fn fields(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

/// What an action was sent: a button posts the page's signals, so it names
/// its row in the address's query; a form posts its fields.
fn sent(query: Option<&str>, body: &[u8]) -> Vec<(String, String)> {
    fields(query.map_or(body, str::as_bytes))
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

/// A job's status as a person would say it.
fn status_label(status: &str) -> &'static str {
    match status {
        "review" => "needs a choice",
        "suspect" => "check quality",
        "importing" => "filing",
        "waiting" => "waiting",
        "downloading" => "downloading",
        "imported" => "in library",
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "working",
    }
}

/// A state the server reports (a job's, a peer's, the session's) as a status in its tone.
fn state<'a>(state: &str, word: impl Into<kit::Text<'a>>) -> kit::Status<'a> {
    let tone = match state {
        "connected" | "online" | "keep" | "imported" => kit::Tone::Ok,
        "displaced" | "disconnected" | "rejected" | "failed" | "suspect" => kit::Tone::Bad,
        "spare" | "away" | "review" | "downloading" | "importing" | "waiting" => kit::Tone::Warn,
        _ => kit::Tone::Wait,
    };
    kit::Status::new(tone, word).busy(state == "connecting")
}

/// The last part of a peer's path.
fn leaf(name: &str) -> &str {
    name.rsplit(['\\', '/']).next().unwrap_or(name)
}

/// A peer's folder as a heading: its name and the one above, which is
/// usually the artist.
fn folder_heading(folder: &str) -> Option<String> {
    let parts: Vec<_> = folder
        .rsplit(['\\', '/'])
        .filter(|p| !p.is_empty())
        .take(2)
        .collect();
    match parts.as_slice() {
        [album, parent] => Some(format!("{parent} / {album}")),
        [album] => Some(album.to_string()),
        _ => None,
    }
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

/// The address of an action on one row of a list. A button posts the page's
/// signals, not a form, so the row it acts on travels in the query.
fn row_url(path: &str, key: &str, value: &str) -> String {
    place_url(path, &[(key, value)])
}

/// Where the page now is, for the address bar. Every action that moves
/// within a page reports it, so a reload, a shared link or back and forward
/// return to the same place: the address is the one that draws it.
fn place(url: &str) -> Event {
    signals(serde_json::json!({ "url": url }))
}

/// The view an address describes; one that does not parse (a repeated key,
/// a value of the wrong type) is the default view rather than an error.
fn query<T: serde::de::DeserializeOwned + Default>(uri: &http::Uri) -> T {
    axum_extra::extract::Query::try_from_uri(uri).map_or_else(|_| T::default(), |q| q.0)
}

/// Rows shown at first, and added by each "Show more", in the lists that
/// grow without end: each kind of job, and upload history. A phone browser
/// cannot hold many thousands of them.
const PAGE: i64 = 100;

/// The most rows a page may ask for, whatever its signals say: fifty pages.
const MOST: i64 = 5_000;
