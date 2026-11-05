//! Search tabs. Each search the page is given opens a tab of its own, kept
//! in the database with the best of what it found, so several searches can
//! be read one at a time and survive a restart. A search runs in a task of
//! its own and keeps its results when it finishes, whether or not a page is
//! watching; the page streams it while it runs and draws the kept results
//! after. Results older than the network's ten-minute reuse are marked stale
//! and can be searched again.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::SearchResponse;

use super::{UiState, browse, escape, failed, human, one, patch, place_url, shown_of, sse};
use crate::App;
use crate::db::{self, SearchTab};
use crate::folders::{Filter, Folder};
use crate::graphql;

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/search/tabs/{id}/pin", post(pin))
        .route("/search/tabs/{id}/close", post(close))
        .route("/search/tabs/{id}/again", post(again))
        .route("/search/tabs/close-unpinned", post(close_unpinned))
}

/// A search page's address: `tab` names the tab shown and `shown` how many
/// of its folders are drawn. `q` (one search per line) and `lossless` open
/// tabs and are replaced by the first of them.
#[derive(serde::Deserialize, Default)]
pub(super) struct SearchQuery {
    #[serde(default)]
    pub q: String,
    /// Any value means lossless, as the page reads it.
    #[serde(default)]
    pub lossless: String,
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    shown: String,
}

/// Folders shown at first, and added by each "Show more". A generic query is
/// answered with thousands, and a page drawing them all every second stops
/// responding.
const FOUND: usize = 100;

/// The most folders a tab keeps, best first.
const KEEP: usize = 500;

/// How long the web UI's searches listen for peers: as long as a grab, so
/// either stands in for the other.
pub(super) const LISTEN: u64 = 10;

/// Results older than this are stale: the network's reuse of the same words
/// has lapsed, so searching again asks the peers afresh.
const STALE: Duration = Duration::from_secs(600);

/// Open a tab for each line of `text`, or bring back the one already open for
/// those words and filter, searching it again if it is stale or failed.
/// Returns the first.
pub(super) async fn open(app: &Arc<App>, text: &str, lossless: bool) -> anyhow::Result<Option<i64>> {
    let mut first = None;
    let mut seen = HashSet::new();
    for words in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !seen.insert(words.to_lowercase()) {
            continue;
        }
        let id = match db::find_search_tab(&app.db, words, lossless).await? {
            Some(tab) => {
                if tab.error.is_some() || stale(&tab) {
                    db::rerun_search_tab(&app.db, tab.id).await?;
                    run(app.clone(), tab.id, tab.query, lossless);
                }
                tab.id
            }
            None => {
                let id = db::open_search_tab(&app.db, words, lossless).await?;
                run(app.clone(), id, words.to_string(), lossless);
                id
            }
        };
        first.get_or_insert(id);
    }
    Ok(first)
}

fn stale(tab: &SearchTab) -> bool {
    tab.searched_at
        .is_some_and(|at| (chrono::Utc::now() - at).to_std().is_ok_and(|d| d > STALE))
}

fn ago(at: chrono::DateTime<chrono::Utc>) -> String {
    let mins = (chrono::Utc::now() - at).num_minutes().max(0);
    match mins {
        0..60 => format!("{mins} min ago"),
        60..1440 => format!("{} h ago", mins / 60),
        _ => format!("{} days ago", mins / 1440),
    }
}

/// Search for the tab in the background and keep what it finds. A tab
/// already searching is left to the search it has.
fn run(app: Arc<App>, id: i64, query: String, lossless: bool) {
    static RUNNING: LazyLock<parking_lot::Mutex<HashSet<i64>>> = LazyLock::new(Default::default);
    if !RUNNING.lock().insert(id) {
        return;
    }
    tokio::spawn(async move {
        let heard = match app.session.engine().cloned() {
            Some(engine) => graphql::responses(&engine, &query, LISTEN)
                .await
                .map_err(|e| format!("{e:#}")),
            None => Err("Sign in to Soulseek first.".to_string()),
        };
        let (responses, found, folders, error) = match heard {
            Ok(responses) => {
                let mut folders = best(&responses, &query, lossless);
                let found = folders.len();
                folders.truncate(KEEP);
                (responses.len(), found, folders, None)
            }
            Err(e) => (0, 0, Vec::new(), Some(e)),
        };
        if let Err(e) = db::finish_search_tab(&app.db, id, responses, found, folders, error).await {
            tracing::warn!(id, error = %e, "could not keep a search's results");
        }
        RUNNING.lock().remove(&id);
    });
}

/// The answers as albums, best first.
fn best(responses: &[Arc<SearchResponse>], query: &str, lossless: bool) -> Vec<Folder> {
    let filter = Filter {
        lossless,
        ..Default::default()
    };
    crate::folders::relevant(
        crate::folders::group(responses.iter().map(Arc::as_ref), &filter),
        query,
    )
}

/// The tab an address names, if it is still open.
pub(super) async fn active(app: &App, view: &SearchQuery) -> Option<SearchTab> {
    let id = view.tab.parse().ok()?;
    db::search_tab(&app.db, id).await.ok().flatten()
}

#[derive(Template)]
#[template(path = "pages/search.html")]
struct SearchPage<'a> {
    /// The finder starts empty: what is typed there opens a new tab.
    q: &'a str,
    lossless: bool,
    tab: Option<&'a SearchTab>,
    tabs: String,
}

pub(super) async fn page(app: &App, tab: Option<&SearchTab>) -> String {
    SearchPage {
        q: "",
        lossless: tab.is_none_or(|t| t.lossless),
        tab,
        tabs: tabs_html(app, tab.map(|t| t.id)).await,
    }
    .render()
    .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "search_tabs.html")]
struct TabsView {
    tabs: Vec<SearchTab>,
    active: Option<i64>,
}

impl TabsView {
    fn stale(&self, t: &SearchTab) -> bool {
        stale(t)
    }
    fn about(&self, t: &SearchTab) -> String {
        let quality = if t.lossless { "lossless" } else { "any quality" };
        match (t.searched_at, &t.error) {
            (None, _) => format!("{} · {quality} · searching", t.query),
            (Some(_), Some(e)) => format!("{} · {quality} · {e}", t.query),
            (Some(at), None) => format!("{} · {quality} · {} folders, searched {}", t.query, t.found, ago(at)),
        }
    }
    fn unpinned(&self) -> usize {
        self.tabs.iter().filter(|t| !t.pinned).count()
    }
}

/// The tab strip, with `active` the tab the page shows.
pub(super) async fn tabs_html(app: &App, active: Option<i64>) -> String {
    match db::search_tabs(&app.db).await {
        Ok(tabs) => TabsView { tabs, active }.render().unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "could not list search tabs");
            String::new()
        }
    }
}

#[derive(Template)]
#[template(path = "results.html")]
struct ResultsView {
    tab: i64,
    query: String,
    folders: Vec<Folder>,
    /// Folders kept beyond those shown.
    more: usize,
    /// The address that shows another page of them.
    next: String,
    searching: bool,
    responses: usize,
    /// Folders found, of which the best `KEEP` are kept.
    found: usize,
    error: Option<String>,
    /// How long ago, once the results are stale.
    age: Option<String>,
}

impl ResultsView {
    fn kept(&self) -> usize {
        KEEP
    }
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

/// The tab's results, the first `shown` of them: what it kept once its
/// search finished, or the search as it runs, regrouped as peers answer at
/// most once a second. A running search is joined rather than sent again,
/// so reopening or reloading the page spends nothing.
pub(super) async fn results(State(s): State<UiState>, Query(view): Query<SearchQuery>) -> Response {
    let Some(tab) = active(&s.app, &view).await else {
        return one(r#"<div id="results" class="text-meta text-muted">This search is closed.</div>"#.into());
    };
    let shown = view.shown.parse().map_or(FOUND, |n: usize| n.clamp(1, KEEP));
    let next = place_url(
        "/search",
        &[("tab", &tab.id.to_string()), ("shown", &(shown + FOUND).to_string())],
    );
    if let Some(at) = tab.searched_at {
        let mut folders = match db::search_tab_folders(&s.app.db, tab.id).await {
            Ok(f) => f,
            Err(e) => return failed(&e.into()),
        };
        let more = folders.len().saturating_sub(shown);
        folders.truncate(shown);
        let view = ResultsView {
            age: stale(&tab).then(|| ago(at)),
            tab: tab.id,
            query: tab.query,
            folders,
            more,
            next,
            searching: false,
            responses: tab.responses as usize,
            found: tab.found as usize,
            error: tab.error,
        };
        return one(view.render().unwrap_or_default());
    }
    let Some(engine) = s.app.session.engine().cloned() else {
        return one(r#"<div id="results" class="text-meta text-muted">Sign in to Soulseek first.</div>"#.into());
    };
    // A search lost to a restart starts again here; one running is joined.
    run(s.app.clone(), tab.id, tab.query.clone(), tab.lossless);
    let events = async_stream::stream! {
        let mut heard = graphql::listen(&engine, &tab.query, LISTEN);
        loop {
            let graphql::Heard { responses, done, error } = heard.borrow_and_update().clone();
            let mut folders = best(&responses, &tab.query, tab.lossless);
            let found = folders.len();
            folders.truncate(KEEP);
            let more = folders.len().saturating_sub(shown);
            folders.truncate(shown);
            let view = ResultsView {
                tab: tab.id,
                query: tab.query.clone(),
                folders,
                more,
                next: next.clone(),
                searching: !done,
                responses: responses.len(),
                found,
                error: error.map(|e| e.to_string()),
                age: None,
            };
            yield Ok(patch(&view.render().unwrap_or_default()));
            if done || heard.changed().await.is_err() {
                break;
            }
        }
    };
    sse(events).into_response()
}

/// Send the browser to `url`, a page of the app's own.
fn go(url: &str) -> Response {
    one(format!(
        r#"<div id="flash" data-init="location.assign('{}')"></div>"#,
        escape(url)
    ))
}

/// The strip redrawn, or the page moved when the tab it showed was closed.
async fn redraw(app: &App, body: &Bytes) -> Response {
    let active = shown_of(body).tab.parse::<i64>().ok();
    let tabs = db::search_tabs(&app.db).await.unwrap_or_default();
    match active {
        Some(id) if !tabs.iter().any(|t| t.id == id) => go(&match tabs.first() {
            Some(t) => format!("/search?tab={}", t.id),
            None => "/search".to_string(),
        }),
        _ => one(TabsView { tabs, active }.render().unwrap_or_default()),
    }
}

async fn pin(State(s): State<UiState>, Path(id): Path<i64>, body: Bytes) -> Response {
    let pinned = match db::search_tab(&s.app.db, id).await {
        Ok(Some(t)) => !t.pinned,
        Ok(None) => return redraw(&s.app, &body).await,
        Err(e) => return failed(&e.into()),
    };
    if let Err(e) = db::pin_search_tab(&s.app.db, id, pinned).await {
        return failed(&e.into());
    }
    redraw(&s.app, &body).await
}

/// Closing the tab the page shows moves to the one after it, or else the
/// one before.
async fn close(State(s): State<UiState>, Path(id): Path<i64>, body: Bytes) -> Response {
    let tabs = db::search_tabs(&s.app.db).await.unwrap_or_default();
    if let Err(e) = db::close_search_tabs(&s.app.db, Some(id)).await {
        return failed(&e.into());
    }
    let active = shown_of(&body).tab.parse::<i64>().ok();
    if active == Some(id)
        && let Some(i) = tabs.iter().position(|t| t.id == id)
    {
        let neighbour = tabs.get(i + 1).or(i.checked_sub(1).and_then(|i| tabs.get(i)));
        return go(&neighbour.map_or("/search".to_string(), |t| format!("/search?tab={}", t.id)));
    }
    redraw(&s.app, &body).await
}

async fn close_unpinned(State(s): State<UiState>, body: Bytes) -> Response {
    if let Err(e) = db::close_search_tabs(&s.app.db, None).await {
        return failed(&e.into());
    }
    redraw(&s.app, &body).await
}

async fn again(State(s): State<UiState>, Path(id): Path<i64>) -> Response {
    let tab = match db::search_tab(&s.app.db, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return go("/search"),
        Err(e) => return failed(&e.into()),
    };
    if let Err(e) = db::rerun_search_tab(&s.app.db, id).await {
        return failed(&e.into());
    }
    run(s.app.clone(), id, tab.query, tab.lossless);
    go(&format!("/search?tab={id}"))
}
