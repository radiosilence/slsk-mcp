//! The Search tab. Each search the page is given opens a tab of its own,
//! kept in the database with the best of what it found, so several searches
//! can be read one at a time and survive a restart. A search runs in a task
//! of its own and keeps its results when it finishes, whether or not a page
//! is watching; the page's stream draws it while it runs, and the page draws
//! the kept results after. Results older than the network's ten-minute reuse
//! are marked stale and can be searched again.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use askama::Template;
use askama_datastar::Html;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use koan_kit_web as kit;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::SearchResponse;
use uuid::Uuid;

use super::{Body, Page, Tab, View};
use crate::App;
use crate::db::{self, SearchTab};
use crate::folders::{Filter, Folder};
use crate::graphql;
use crate::web::stream::Shown;
use crate::web::{Draw, UiState, failed, field, fields, human, place_url};

/// Folders shown at first, and added by each "Show more". A generic query is
/// answered with thousands, and a page drawing them all every second stops
/// responding.
const FOUND: usize = 100;

/// The most folders a tab keeps, best first.
const KEEP: usize = 500;

/// How long the web UI's searches listen for peers: as long as a grab, so
/// either stands in for the other.
pub(in crate::web) const LISTEN: u64 = 10;

/// Results older than this are stale: the network's reuse of the same words
/// has lapsed, so searching again asks the peers afresh.
const STALE: Duration = Duration::from_secs(600);

pub(in crate::web) struct Search;

/// A search page's address: `tab` names the tab shown and `shown` how many
/// of its folders are drawn. `q` (any number of them, each one search per
/// line) and `lossless` open tabs and are replaced by the first of them.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(in crate::web) struct SearchQuery {
    q: Vec<String>,
    /// Any value means lossless, as the page reads it.
    lossless: String,
    tab: String,
    shown: String,
}

impl SearchQuery {
    /// How many folders the address asks to show.
    pub(in crate::web) fn shown(&self) -> usize {
        self.shown
            .parse()
            .map_or(FOUND, |n: usize| n.clamp(1, KEEP))
    }

    pub(in crate::web) fn tab(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.tab).ok()
    }
}

#[derive(Template)]
#[template(path = "search/page.html")]
struct SearchPage<'a> {
    /// The finder starts empty: what is typed there opens a new tab.
    q: &'a str,
    lossless: bool,
    /// Searches were sent, every one of them empty.
    blank: bool,
    tabs: Html,
    /// The tab's results, as kept or as heard so far.
    results: Option<Html>,
}

impl Page for Search {
    const TAB: Tab = Tab::Search;
    const STREAM: &'static str = "/stream/search";
    type Args = View<SearchQuery>;

    async fn body(s: &UiState, View(view): View<SearchQuery>) -> anyhow::Result<Body> {
        // A search typed or pasted opens its tabs, and the page moves to the first.
        if view.q.iter().any(|q| !q.trim().is_empty()) {
            let first = open(s, &view.q.join("\n"), !view.lossless.is_empty()).await?;
            return Ok(Body::redirect(
                first.map_or("/search".to_string(), |id| format!("/search?tab={id}")),
            ));
        }
        let tab = match view.tab() {
            Some(id) => db::search_tab(&s.app.db, id).await?,
            None => None,
        };
        let results = match (&tab, s.app.session.engine()) {
            (Some(t), Some(engine)) if t.searched_at.is_some() => {
                let results = Results::kept(s, t, view.shown()).await;
                // Kept results can be hours old: who is online is asked anew.
                for user in results.users() {
                    s.peers.ask_status(engine, user);
                }
                Some(results.draw())
            }
            (Some(t), None) if t.searched_at.is_some() => {
                Some(Results::kept(s, t, view.shown()).await.draw())
            }
            // A search lost to a restart starts again here; one running is
            // joined, and the stream draws it as peers answer.
            (Some(t), Some(engine)) => {
                run(s, t.id, t.query.clone(), t.lossless);
                let heard = graphql::listen(engine, &t.query, LISTEN).borrow().clone();
                Some(Results::heard(s, engine, t, &heard, view.shown()).draw())
            }
            _ => None,
        };
        let heading = tab.as_ref().map(|t| t.query.clone());
        Ok(Body::of(&SearchPage {
            q: "",
            lossless: tab.as_ref().is_none_or(|t| t.lossless),
            blank: !view.q.is_empty(),
            tabs: Tabs::of(&s.app, view.tab()).await.draw(),
            results,
        })
        .heading(heading))
    }
}

/// Open a tab for each line of `text`, or bring back the one already open for
/// those words and filter, searching it again if it is stale or failed.
/// Returns the first.
async fn open(s: &UiState, text: &str, lossless: bool) -> anyhow::Result<Option<Uuid>> {
    let app = &s.app;
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
                    run(s, tab.id, tab.query, lossless);
                }
                tab.id
            }
            None => {
                let id = db::open_search_tab(&app.db, words, lossless).await?;
                run(s, id, words.to_string(), lossless);
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

/// How long ago, as text for a tab's title, an attribute that cannot hold a [`kit::Time`].
fn title_ago(at: chrono::DateTime<chrono::Utc>) -> String {
    let mins = (chrono::Utc::now() - at).num_minutes().max(0);
    match mins {
        0 => "just now".to_string(),
        1..60 => format!("{mins} min ago"),
        60..1440 => format!("{} h ago", mins / 60),
        _ => format!("{} days ago", mins / 1440),
    }
}

/// Search for the tab in the background and keep what it finds, then ask
/// whether the peers behind the best of it are online. A tab already
/// searching is left to the search it has.
pub(in crate::web) fn run(s: &UiState, id: Uuid, query: String, lossless: bool) {
    static RUNNING: LazyLock<parking_lot::Mutex<HashSet<Uuid>>> = LazyLock::new(Default::default);
    if !RUNNING.lock().insert(id) {
        return;
    }
    let (app, peers) = (s.app.clone(), s.peers.clone());
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
        if let Some(engine) = app.session.engine() {
            let users: HashSet<&str> = folders
                .iter()
                .take(FOUND)
                .map(|f| f.username.as_str())
                .collect();
            for user in users {
                peers.ask_status(engine, user);
            }
        }
        if let Err(e) = db::finish_search_tab(&app.db, id, responses, found, folders, error).await {
            tracing::warn!(%id, error = %e, "could not keep a search's results");
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

/// The tab strip, with `active` the tab the page shows.
#[derive(Template)]
#[template(path = "search/tabs.html")]
pub(in crate::web) struct Tabs {
    tabs: Vec<SearchTab>,
    active: Option<Uuid>,
    /// The database did not answer, so these are not the tabs as they stand.
    pub(in crate::web) failed: bool,
}

impl Tabs {
    pub(in crate::web) async fn of(app: &App, active: Option<Uuid>) -> Self {
        let read = db::search_tabs(&app.db)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "could not list search tabs"));
        let failed = read.is_err();
        Self {
            tabs: read.unwrap_or_default(),
            active,
            failed,
        }
    }
    fn is_active(&self, t: &SearchTab) -> bool {
        self.active == Some(t.id)
    }
    fn href(&self, t: &SearchTab) -> String {
        format!("/search?tab={}", t.id)
    }
    fn failed(&self, t: &SearchTab) -> bool {
        t.error.is_some()
    }
    /// Slanted once its results are stale; the kit has no tone for that.
    fn slant(&self, t: &SearchTab) -> &'static str {
        if t.error.is_none() && t.searched_at.is_some() && stale(t) {
            "[&>a]:italic"
        } else {
            ""
        }
    }
    /// Still searching: the tab's ensō turns. A failed search is never still searching.
    fn searching(&self, t: &SearchTab) -> bool {
        t.searched_at.is_none() && t.error.is_none()
    }
    /// How many folders a search that finished without failing found.
    fn state(&self, t: &SearchTab) -> String {
        match (t.searched_at, &t.error) {
            (Some(_), None) => t.found.to_string(),
            _ => String::new(),
        }
    }
    fn pin_label(&self, t: &SearchTab) -> String {
        if t.pinned {
            format!("Unpin {}", t.query)
        } else {
            format!("Pin {}", t.query)
        }
    }
    fn about(&self, t: &SearchTab) -> String {
        let quality = if t.lossless {
            "lossless"
        } else {
            "any quality"
        };
        match (t.searched_at, &t.error) {
            (None, _) => format!("{} · {quality} · searching", t.query),
            (Some(_), Some(e)) => format!("{} · {quality} · {e}", t.query),
            (Some(at), None) => format!(
                "{} · {quality} · {} folders, searched {}",
                t.query,
                t.found,
                title_ago(at)
            ),
        }
    }
    fn unpinned(&self) -> usize {
        self.tabs.iter().filter(|t| !t.pinned).count()
    }
}

/// The folders a tab found, best first, the first `shown` of them.
#[derive(Template)]
#[template(path = "search/results.html")]
pub(in crate::web) struct Results {
    tab: Uuid,
    query: String,
    pub(in crate::web) folders: Vec<Folder>,
    /// Folders kept beyond those shown.
    more: usize,
    /// The address that shows another page of them.
    next: String,
    searching: bool,
    responses: usize,
    /// Folders found, of which the best `KEEP` are kept.
    found: usize,
    error: Option<String>,
    /// When the search finished.
    age: Option<kit::Time<'static>>,
    stale: bool,
    /// Whether the peers behind the folders are online, as far as the
    /// server has said.
    statuses: HashMap<String, &'static str>,
}

impl Results {
    /// A finished tab's results, from what it kept.
    pub(in crate::web) async fn kept(s: &UiState, tab: &SearchTab, shown: usize) -> Self {
        let app = &s.app;
        let mut folders = db::search_tab_folders(&app.db, tab.id)
            .await
            .inspect_err(
                |e| tracing::warn!(id = %tab.id, error = %e, "could not read a search's results"),
            )
            .unwrap_or_default();
        if let Some(engine) = app.session.engine() {
            folders.retain(|f| !engine.is_banned(&f.username));
        }
        let more = folders.len().saturating_sub(shown);
        folders.truncate(shown);
        Self {
            age: tab.searched_at.map(crate::web::ago),
            stale: stale(tab),
            tab: tab.id,
            query: tab.query.clone(),
            folders,
            more,
            next: next(tab.id, shown),
            searching: false,
            responses: tab.responses as usize,
            found: tab.found as usize,
            error: tab.error.clone(),
            statuses: HashMap::new(),
        }
        .with_statuses(s)
    }

    /// A running search's results, as far as peers have answered.
    pub(in crate::web) fn heard(
        s: &UiState,
        engine: &slsk_engine::Engine,
        tab: &SearchTab,
        heard: &graphql::Heard,
        shown: usize,
    ) -> Self {
        let responses = graphql::unbanned(engine, &heard.responses);
        let mut folders = best(&responses, &tab.query, tab.lossless);
        let found = folders.len();
        folders.truncate(KEEP);
        let more = folders.len().saturating_sub(shown);
        folders.truncate(shown);
        Self {
            tab: tab.id,
            query: tab.query.clone(),
            folders,
            more,
            next: next(tab.id, shown),
            searching: !heard.done,
            responses: responses.len(),
            found,
            error: heard.error.as_ref().map(|e| e.to_string()),
            age: heard.finished.map(crate::web::ago),
            stale: false,
            statuses: HashMap::new(),
        }
        .with_statuses(s)
    }

    /// Who is online among the peers behind the folders, as answered so far.
    fn with_statuses(mut self, s: &UiState) -> Self {
        self.statuses = self
            .users()
            .filter_map(|u| Some((u.to_string(), s.peers.status.answer(u)?.ok()?)))
            .collect();
        self
    }

    /// The peers behind the folders shown.
    pub(in crate::web) fn users(&self) -> impl Iterator<Item = &str> {
        self.folders
            .iter()
            .map(|f| f.username.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
    }

    fn status(&self, f: &Folder) -> Option<&'static str> {
        self.statuses.get(&f.username).copied()
    }

    fn kept_most(&self) -> usize {
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
        folder_id(f)
    }
    fn crumbs(&self, f: &Folder) -> Vec<super::browse::Child> {
        super::browse::crumbs(f.remote_path.as_bytes())
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

/// The address that shows a page more of the tab's folders.
fn next(tab: Uuid, shown: usize) -> String {
    place_url(
        "/search",
        &[
            ("tab", &tab.to_string()),
            ("shown", &(shown + FOUND).to_string()),
        ],
    )
}

/// Stable across regroups, so an open preview stays with its folder.
fn folder_id(f: &Folder) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    (&f.username, f.remote_path.as_bytes()).hash(&mut h);
    h.finish()
}

/// Send the browser to `url`, a page of the app's own.
fn go(url: &str) -> Response {
    askama_datastar::events([askama_datastar::go(url).into()])
}

/// The tab changed; the page's stream draws the strip.
fn changed() -> Response {
    askama_datastar::events([
        kit::Flash::region().patch().into(),
        kit::Confirm::close().into(),
    ])
}

pub(in crate::web) async fn pin(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    let pinned = match db::search_tab(&s.app.db, id).await {
        Ok(Some(t)) => !t.pinned,
        Ok(None) => return changed(),
        Err(e) => return failed(&e.into()),
    };
    match db::pin_search_tab(&s.app.db, id, pinned).await {
        Ok(()) => changed(),
        Err(e) => failed(&e.into()),
    }
}

/// Closing the tab the page shows moves to the one after it, or else the
/// one before.
pub(in crate::web) async fn close(
    State(s): State<UiState>,
    Path(id): Path<Uuid>,
    askama_datastar::Signals(shown): askama_datastar::Signals<Shown>,
) -> Response {
    let tabs = db::search_tabs(&s.app.db).await.unwrap_or_default();
    if let Err(e) = db::close_search_tabs(&s.app.db, Some(id)).await {
        return failed(&e.into());
    }
    if shown.view::<SearchQuery>().tab() == Some(id)
        && let Some(i) = tabs.iter().position(|t| t.id == id)
    {
        let neighbour = tabs
            .get(i + 1)
            .or(i.checked_sub(1).and_then(|i| tabs.get(i)));
        return go(&neighbour.map_or("/search".to_string(), |t| format!("/search?tab={}", t.id)));
    }
    changed()
}

/// Asked first, in the page's question, which closes once it is done.
pub(in crate::web) async fn close_unpinned(
    State(s): State<UiState>,
    askama_datastar::Signals(shown): askama_datastar::Signals<Shown>,
) -> Response {
    if let Err(e) = db::close_search_tabs(&s.app.db, None).await {
        return failed(&e.into());
    }
    let open = db::search_tabs(&s.app.db).await.unwrap_or_default();
    match shown.view::<SearchQuery>().tab() {
        Some(id) if !open.iter().any(|t| t.id == id) => go(&match open.first() {
            Some(t) => format!("/search?tab={}", t.id),
            None => "/search".to_string(),
        }),
        _ => changed(),
    }
}

pub(in crate::web) async fn again(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    let tab = match db::search_tab(&s.app.db, id).await {
        Ok(Some(t)) => t,
        Ok(None) => return go("/search"),
        Err(e) => return failed(&e.into()),
    };
    if let Err(e) = db::rerun_search_tab(&s.app.db, id).await {
        return failed(&e.into());
    }
    graphql::forget_answers(&tab.query);
    run(&s, id, tab.query, tab.lossless);
    go(&format!("/search?tab={id}"))
}

/// A folder from the results downloaded. The files the results listed come
/// with it, to fetch by name when the peer will not list the folder.
pub(in crate::web) async fn download(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let (Some(username), Some(Ok(raw))) = (
        field(&form, "username"),
        field(&form, "key").map(|k| URL_SAFE_NO_PAD.decode(k)),
    ) else {
        return http::StatusCode::BAD_REQUEST.into_response();
    };
    let files = form
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
    let title = field(&form, "title").map(str::to_string);
    match s.app.jobs.from_folder(&folder, title, Vec::new()).await {
        Ok(_) => {
            let name = crate::web::folder_heading(&folder.path).unwrap_or(folder.path.clone());
            askama_datastar::events([
                kit::Flash::ok(format!("Downloading {name} from {username}"))
                    .patch()
                    .into(),
            ])
        }
        Err(e) => failed(&e),
    }
}
