//! Browsing a user's shares, or this client's own, as a tree: the folder in
//! the address is the open one, and the folders under it expand in place,
//! the expanded ones named in the address too. Any folder or file in the
//! tree can be downloaded from its own row.
//!
//! A peer sends its whole share list at once, which for a large collection
//! is tens of thousands of folders and takes a while, so the last few lists
//! fetched are kept for a short time and each step through them is local.
//! Folders are named in URLs and forms by the base64 of their raw bytes:
//! peer paths are not always UTF-8, and a key is safe anywhere.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum_extra::extract::{Form, Query};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::Directory;

use super::{UiState, failed, fields, flash_ok, human, many, one, patch, sse};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/browse", post(browse).get(browse_get))
        .route("/browse/files", post(download_files))
        .route("/browse/tree", post(download_tree))
}

type Listing = Arc<Vec<Directory>>;

/// Peers' listings, and this client's own apart from them: its own is
/// browsed but never downloaded from, so it neither takes a peer's slot nor
/// answers a download.
#[derive(Clone, Default)]
pub(super) struct Cache {
    peers: Arc<parking_lot::Mutex<Vec<(String, Instant, Listing)>>>,
    own: Arc<parking_lot::Mutex<Option<(Instant, Listing)>>>,
}

const KEEP: usize = 4;
const FRESH: Duration = Duration::from_secs(600);
/// Imports change the library often, so its own listing is read again
/// sooner; between reads each step through it is a lookup, not a copy of
/// the share index.
const OWN_FRESH: Duration = Duration::from_secs(60);

impl Cache {
    fn own(&self, engine: &slsk_engine::Engine, fresh: bool) -> Listing {
        let mut c = self.own.lock();
        match &*c {
            Some((at, l)) if !fresh && at.elapsed() < OWN_FRESH => l.clone(),
            _ => {
                let l: Listing = Arc::new(engine.own_shares());
                *c = Some((Instant::now(), l.clone()));
                l
            }
        }
    }

    fn get(&self, username: &str) -> Option<Listing> {
        let mut c = self.peers.lock();
        c.retain(|(_, at, _)| at.elapsed() < FRESH);
        c.iter()
            .find(|(u, ..)| u == username)
            .map(|(.., l)| l.clone())
    }

    fn put(&self, username: &str, listing: Listing) {
        let mut c = self.peers.lock();
        c.retain(|(u, ..)| u != username);
        c.push((username.to_string(), Instant::now(), listing));
        if c.len() > KEEP {
            c.remove(0);
        }
    }
}

fn key(raw: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(raw)
}

pub(super) struct Child {
    path: Vec<u8>,
    pub(super) key: String,
    pub(super) name: String,
    /// Files in it and every folder under it.
    files: usize,
}

/// The folders directly under `at` (the top level when empty), and `at`
/// itself when the peer listed it. Peers list only the folders holding
/// files, so intermediate folders are inferred from the paths below them.
fn level<'a>(dirs: &'a [Directory], at: &[u8]) -> (Vec<Child>, Option<&'a Directory>) {
    let mut prefix = at.to_vec();
    if !prefix.is_empty() {
        prefix.push(b'\\');
    }
    let mut children: std::collections::BTreeMap<Vec<u8>, usize> = Default::default();
    let mut here = None;
    for d in dirs {
        let name = d.name.as_bytes();
        if name == at {
            here = Some(d);
            continue;
        }
        let Some(rest) = name.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        let first = rest.split(|b| *b == b'\\').next().unwrap_or_default();
        if first.is_empty() {
            continue;
        }
        let mut path = prefix.clone();
        path.extend_from_slice(first);
        *children.entry(path).or_default() += d.files.len();
    }
    let children = children
        .into_iter()
        .map(|(path, files)| Child {
            key: key(&path),
            name: RawStr(path[prefix.len()..].to_vec().into()).to_string_lossy(),
            files,
            path,
        })
        .collect();
    (children, here)
}

/// Each folder from the top down to `at`, for going back up.
pub(super) fn crumbs(at: &[u8]) -> Vec<Child> {
    let mut out = Vec::new();
    if at.is_empty() {
        return out;
    }
    let mut path = Vec::new();
    for part in at.split(|b| *b == b'\\') {
        if !path.is_empty() {
            path.push(b'\\');
        }
        path.extend_from_slice(part);
        out.push(Child {
            path: path.clone(),
            key: key(&path),
            name: RawStr(part.to_vec().into()).to_string_lossy(),
            files: 0,
        });
    }
    out
}

pub(super) struct FileRow {
    key: String,
    name: String,
    size: u64,
    quality: String,
}

struct Info {
    description: String,
    uploads: u32,
    queue: u32,
    free: bool,
}

/// Where a user's tree is: the open folder, the folders expanded under it
/// and how many extra pages of a folder's rows are shown. All of it is in
/// the address, so a reload, a link, or back and forward draw the same tree.
/// Expanded folders are named relative to the open one (`open=A&open=A/B`,
/// `.` for the open folder itself in `more`), which keeps the address
/// readable and lets it survive a move up or down.
#[derive(Clone, Debug, Default, PartialEq)]
struct Place {
    username: String,
    at: Vec<u8>,
    open: BTreeSet<Vec<u8>>,
    more: BTreeMap<Vec<u8>, usize>,
}

/// `path` lies below `dir`, at any depth.
fn under(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return !path.is_empty();
    }
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'\\'
}

impl Place {
    fn from_query(q: &BrowseQuery) -> anyhow::Result<Self> {
        let username = q.username.trim().to_string();
        if username.is_empty() {
            anyhow::bail!("Say whose shares to browse.");
        }
        let at = URL_SAFE_NO_PAD
            .decode(&q.key)
            .map_err(|_| anyhow::anyhow!("not a folder key"))?;
        let open = q.open.iter().map(|r| join(&at, r)).collect();
        let mut more = BTreeMap::new();
        for r in &q.more {
            *more.entry(join(&at, r)).or_default() += 1;
        }
        Ok(Self {
            username,
            at,
            open,
            more,
        })
    }

    fn rel(&self, path: &[u8]) -> String {
        if path == self.at.as_slice() {
            return ".".into();
        }
        let start = if self.at.is_empty() {
            0
        } else {
            self.at.len() + 1
        };
        let rel = &path[start..];
        match std::str::from_utf8(rel) {
            Ok(r) if !r.contains('/') && !r.starts_with(RAW) => r.replace('\\', "/"),
            _ => format!("{RAW}{}", URL_SAFE_NO_PAD.encode(rel)),
        }
    }

    fn href(&self) -> String {
        let key = key(&self.at);
        let mut params = vec![("username", self.username.clone()), ("key", key)];
        params.extend(self.open.iter().map(|p| ("open", self.rel(p))));
        for (p, n) in &self.more {
            params.extend(std::iter::repeat_n(("more", self.rel(p)), *n));
        }
        let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        super::place_url("/browse", &params)
    }

    /// The same tree with `path` expanded, or collapsed along with every
    /// folder and page under it.
    fn toggle(&self, path: &[u8]) -> String {
        let mut p = self.clone();
        if !p.open.remove(path) {
            p.open.insert(path.to_vec());
        } else {
            p.open.retain(|o| !under(o, path));
            p.more.retain(|m, _| m != path && !under(m, path));
        }
        p.href()
    }

    /// The same tree with a page more of `path`'s rows.
    fn show_more(&self, path: &[u8]) -> String {
        let mut p = self.clone();
        *p.more.entry(path.to_vec()).or_default() += 1;
        p.href()
    }

    /// `target` as the open folder. What was expanded below it stays
    /// expanded, and going up leaves the way back down expanded, as Finder
    /// does; pages shown start again.
    fn go(&self, target: &[u8]) -> String {
        let mut open: BTreeSet<Vec<u8>> = self.open.clone();
        if under(&self.at, target) {
            open.insert(self.at.clone());
            open.extend(
                self.at
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'\\')
                    .map(|(i, _)| self.at[..i].to_vec()),
            );
        }
        open.retain(|o| under(o, target));
        Self {
            username: self.username.clone(),
            at: target.to_vec(),
            open,
            more: BTreeMap::new(),
        }
        .href()
    }
}

/// Marks a relative path the address cannot spell as itself: one that is
/// not UTF-8 or holds a `/`, given instead as the base64 of its bytes.
const RAW: &str = "~";

/// A path named relative to `at`, back in the peer's own form.
fn join(at: &[u8], rel: &str) -> Vec<u8> {
    if rel == "." || rel.is_empty() {
        return at.to_vec();
    }
    let rel = match rel.strip_prefix(RAW) {
        Some(b) => URL_SAFE_NO_PAD.decode(b).unwrap_or_default(),
        None => rel.replace('/', "\\").into_bytes(),
    };
    let mut out = at.to_vec();
    if !out.is_empty() {
        out.push(b'\\');
    }
    out.extend_from_slice(&rel);
    out
}

pub(super) struct FolderRow {
    name: String,
    key: String,
    /// Files in it and every folder under it.
    files: usize,
    go: String,
    toggle: String,
    /// Its own rows, drawn when it is expanded.
    inner: Option<String>,
}

pub(super) enum Node {
    Folder(FolderRow),
    File(FileRow),
}

/// The rows of one folder in the tree: its folders, then its files, a page
/// at a time so a folder of thousands stays light to draw. Folders expanded
/// in the address are drawn with their own rows beneath them.
#[derive(Template)]
#[template(path = "browse_nodes.html")]
struct Nodes<'a> {
    username: &'a str,
    nodes: Vec<Node>,
    /// The address with another page of them, when there is one.
    more: Option<String>,
    /// Fewer drawn than the page holds, as the tree's other open folders
    /// used up [`MOST_ROWS`].
    crowded: bool,
    /// They are the open folder's own, whose files can be ticked.
    top: bool,
    /// This client's own shares, which there is nothing to download from.
    mine: bool,
}

impl Nodes<'_> {
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
}

/// The most rows one tree draws, however many folders are expanded: past
/// it, an expanded folder asks for others to be collapsed.
const MOST_ROWS: usize = 5000;

fn tree(p: &Place, dirs: &[Directory], at: &[u8], mine: bool, left: &mut usize) -> String {
    let (children, here) = level(dirs, at);
    let files = here.into_iter().flat_map(|d| {
        d.files.iter().map(|f| {
            let mut full = at.to_vec();
            full.push(b'\\');
            full.extend_from_slice(f.name.as_bytes());
            Node::File(FileRow {
                key: key(&full),
                name: f.name.to_string_lossy(),
                size: f.size,
                quality: quality(f),
            })
        })
    });
    let paged = super::PAGE as usize * (1 + p.more.get(at).copied().unwrap_or(0));
    let total = children.len() + here.map_or(0, |d| d.files.len());
    let shown = paged.min(total).min(*left);
    *left -= shown;
    let folders = children.len();
    let mut nodes: Vec<Node> = children
        .into_iter()
        .take(shown)
        .map(|c| {
            Node::Folder(FolderRow {
                go: p.go(&c.path),
                toggle: p.toggle(&c.path),
                inner: p
                    .open
                    .contains(&c.path)
                    .then(|| tree(p, dirs, &c.path, mine, left)),
                name: c.name,
                key: c.key,
                files: c.files,
            })
        })
        .collect();
    nodes.extend(files.take(shown.saturating_sub(folders)));
    Nodes {
        username: &p.username,
        nodes,
        crowded: shown < paged.min(total),
        more: (shown == paged && total > paged).then(|| p.show_more(at)),
        top: at == p.at.as_slice(),
        mine,
    }
    .render()
    .unwrap_or_default()
}

struct Crumb {
    name: String,
    href: String,
}

#[derive(Template)]
#[template(path = "browse.html")]
struct BrowseView {
    username: String,
    mine: bool,
    folders: usize,
    /// The top, then each folder down to the open one.
    crumbs: Vec<Crumb>,
    /// The folder above the open one, when it is not the top.
    up: Option<String>,
    refresh: String,
    here_key: String,
    /// Files in this folder and every folder under it.
    tree_files: usize,
    /// The open folder holds files of its own.
    has_files: bool,
    /// The open folder's rows, drawn by [`Nodes`].
    rows: String,
    info: Option<Info>,
    error: Option<String>,
}

fn quality(f: &slsk_engine::slsk_proto::peer::FileEntry) -> String {
    use slsk_engine::slsk_proto::peer::FileEntry as F;
    let ext = f.extension.to_lowercase();
    match (
        f.attr(F::BIT_DEPTH),
        f.attr(F::SAMPLE_RATE),
        f.attr(F::BITRATE),
    ) {
        (Some(d), Some(r), _) => format!("{ext} {d}/{}", r as f64 / 1000.0),
        (_, _, Some(b)) => format!("{ext} {b} kbps"),
        _ => ext,
    }
}

fn render(p: &Place, dirs: &[Directory], info: Option<Info>, mine: bool) -> String {
    let mut crumbs = vec![Crumb {
        name: "top".into(),
        href: p.go(b""),
    }];
    crumbs.extend(self::crumbs(&p.at).into_iter().map(|c| Crumb {
        href: p.go(&c.path),
        name: c.name,
    }));
    let up = (crumbs.len() > 1).then(|| crumbs[crumbs.len() - 2].href.clone());
    BrowseView {
        username: p.username.clone(),
        mine,
        folders: dirs.len(),
        crumbs,
        up,
        refresh: format!("{}&fresh=1", p.href()),
        here_key: key(&p.at),
        tree_files: albums(dirs, &p.at).values().map(Vec::len).sum(),
        has_files: dirs
            .iter()
            .any(|d| d.name.as_bytes() == p.at.as_slice() && !d.files.is_empty()),
        rows: tree(p, dirs, &p.at, mine, &mut { MOST_ROWS }),
        info,
        error: None,
    }
    .render()
    .unwrap_or_default()
}

fn note(username: &str, error: String) -> String {
    BrowseView {
        username: username.to_string(),
        mine: false,
        folders: 0,
        crumbs: Vec::new(),
        up: None,
        refresh: String::new(),
        here_key: String::new(),
        tree_files: 0,
        has_files: false,
        rows: String::new(),
        info: None,
        error: Some(error),
    }
    .render()
    .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "pages/browse.html")]
struct Page<'a> {
    username: &'a str,
    /// The account signed in, whose own shares can be browsed too.
    me: Option<String>,
}

/// The page around a user's shares, which it fetches once drawn: the peer
/// takes its time to answer.
pub(super) fn page(s: &UiState, username: &str) -> String {
    Page {
        username: username.trim(),
        me: s.app.session.engine().map(|e| e.username()),
    }
    .render()
    .unwrap_or_default()
}

/// A tree's place as the address names it: see [`Place`]. `fresh` asks the
/// peer for their shares again rather than using the cached listing.
#[derive(serde::Deserialize, Default)]
struct BrowseQuery {
    #[serde(default)]
    username: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    open: Vec<String>,
    #[serde(default)]
    more: Vec<String>,
    #[serde(default)]
    fresh: Option<String>,
}

/// Open a user's shares at their top. The username arrives in a form
/// field, never in the URL or an expression.
async fn browse(State(s): State<UiState>, Form(q): Form<BrowseQuery>) -> Response {
    browse_at(s, &q)
}

/// The same, from the address: `/browse?username=…&key=…&open=…`, which
/// `browse` reports as the place it drew.
async fn browse_get(State(s): State<UiState>, Query(q): Query<BrowseQuery>) -> Response {
    browse_at(s, &q)
}

fn browse_at(s: UiState, q: &BrowseQuery) -> Response {
    let fresh = q.fresh.is_some();
    let p = match Place::from_query(q) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let here = super::place(&p.href());
    let Some(engine) = s.app.session.engine().cloned() else {
        return failed(&anyhow::anyhow!("Sign in to Soulseek first."));
    };
    let cache = s.browsed.clone();
    if engine.username() == p.username {
        let listing = cache.own(&engine, fresh);
        return many(vec![
            here,
            patch(&render(&p, &listing, None, true)),
            patch("<div id=\"flash\"></div>"),
        ]);
    }
    sse(async_stream::stream! {
        let username = p.username.clone();
        yield Ok(here);
        let listing = match cache.get(&username).filter(|_| !fresh) {
            Some(l) => l,
            None => {
                yield Ok(patch(&note(&username, format!("Asking {username} for their shares…"))));
                let info = tokio::time::timeout(Duration::from_secs(20), engine.user_info(&username));
                let (dirs, info) = tokio::join!(engine.browse(&username), info);
                match dirs {
                    Ok(d) => {
                        let l = Arc::new(d);
                        cache.put(&username, l.clone());
                        let info = info.ok().and_then(Result::ok).map(|i| Info {
                            description: i.description,
                            uploads: i.total_uploads,
                            queue: i.queue_size,
                            free: i.slots_free,
                        });
                        yield Ok(patch(&render(&p, &l, info, false)));
                        return;
                    }
                    Err(e) => {
                        yield Ok(patch(&note(&username, format!("{username} did not send their shares: {e}"))));
                        return;
                    }
                }
            }
        };
        yield Ok(patch(&render(&p, &listing, None, false)));
        yield Ok(patch("<div id=\"flash\"></div>"));
    })
    .into_response()
}

/// Download the ticked files of one folder as an album of their own. Sizes
/// come from the listing the page was drawn from, not from the form.
async fn download_files(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let wanted: Vec<Vec<u8>> = form
        .iter()
        .filter(|(k, _)| k == "f")
        .filter_map(|(_, v)| URL_SAFE_NO_PAD.decode(v).ok())
        .collect();
    // A file downloaded from its own row names no folder: it is the file's.
    let at = match super::field(&form, "key") {
        Some(k) => URL_SAFE_NO_PAD.decode(k).ok(),
        None => wanted.first().map(|f| parent(f).to_vec()),
    };
    let (Some(username), Some(at)) = (super::field(&form, "username"), at) else {
        return failed(&anyhow::anyhow!("no folder given"));
    };
    let Some(listing) = s.browsed.get(username) else {
        return failed(&anyhow::anyhow!(
            "That listing has expired; browse the folder again."
        ));
    };
    let Some(dir) = listing.iter().find(|d| d.name.as_bytes() == at.as_slice()) else {
        return failed(&anyhow::anyhow!("That folder is no longer in the listing."));
    };
    let files: Vec<(RawStr, u64)> = dir
        .files
        .iter()
        .filter_map(|f| {
            let mut full = at.clone();
            full.push(b'\\');
            full.extend_from_slice(f.name.as_bytes());
            wanted
                .contains(&full)
                .then(|| (RawStr(full.into()), f.size))
        })
        .collect();
    if files.is_empty() {
        return failed(&anyhow::anyhow!("Tick the files to download."));
    }
    let title = dir
        .name
        .to_string_lossy()
        .rsplit('\\')
        .next()
        .unwrap_or_default()
        .to_string();
    match s.app.jobs.from_files(username, files, title).await {
        Ok(job) => one(flash_ok(&format!("Downloading {}", job.title))),
        Err(e) => failed(&e),
    }
}

/// The folder a file or folder is in; the top for one at the top.
fn parent(path: &[u8]) -> &[u8] {
    path.iter()
        .rposition(|b| *b == b'\\')
        .map_or(&[][..], |i| &path[..i])
}

/// Every folder at or under `at` that holds files, grouped into albums: a
/// disc folder ("CD1") belongs to its parent, and its name becomes the
/// file's subdirectory so same-named tracks on different discs stay apart.
fn albums(dirs: &[Directory], at: &[u8]) -> BTreeMap<Vec<u8>, Vec<(RawStr, u64, String)>> {
    let mut prefix = at.to_vec();
    if !prefix.is_empty() {
        prefix.push(b'\\');
    }
    let mut out: BTreeMap<Vec<u8>, Vec<(RawStr, u64, String)>> = BTreeMap::new();
    for d in dirs {
        let name = d.name.as_bytes();
        if d.files.is_empty() || !(at.is_empty() || name == at || name.starts_with(&prefix)) {
            continue;
        }
        let (root, sub) = match name.iter().rposition(|b| *b == b'\\') {
            Some(i)
                if i >= at.len()
                    && crate::folders::is_disc(&String::from_utf8_lossy(&name[i + 1..])) =>
            {
                (
                    name[..i].to_vec(),
                    String::from_utf8_lossy(&name[i + 1..]).into_owned(),
                )
            }
            _ => (name.to_vec(), String::new()),
        };
        let files = out.entry(root).or_default();
        for f in &d.files {
            let mut full = name.to_vec();
            full.push(b'\\');
            full.extend_from_slice(f.name.as_bytes());
            files.push((RawStr(full.into()), f.size, sub.clone()));
        }
    }
    out
}

/// More than this is a whole collection, not a folder of albums.
const MAX_ALBUMS: usize = 200;

/// Download a folder and everything under it, one job per album. Files and
/// sizes come from the listing the page was drawn from, not from the form.
async fn download_tree(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let (Some(username), Some(at)) = (
        super::field(&form, "username"),
        super::field(&form, "key").and_then(|k| URL_SAFE_NO_PAD.decode(k).ok()),
    ) else {
        return failed(&anyhow::anyhow!("no folder given"));
    };
    let Some(listing) = s.browsed.get(username) else {
        return failed(&anyhow::anyhow!(
            "That listing has expired; browse the folder again."
        ));
    };
    let albums = albums(&listing, &at);
    if albums.is_empty() {
        return failed(&anyhow::anyhow!("There are no files under that folder."));
    }
    if albums.len() > MAX_ALBUMS {
        return failed(&anyhow::anyhow!(
            "That is {} albums; open a folder with at most {MAX_ALBUMS}.",
            albums.len()
        ));
    }
    let count = albums.len();
    let mut started = Vec::new();
    for (root, files) in albums {
        let title = RawStr(root.into())
            .to_string_lossy()
            .rsplit('\\')
            .next()
            .unwrap_or_default()
            .to_string();
        match s.app.jobs.from_listing(username, files, title).await {
            Ok(job) => started.push(job.title),
            Err(e) => {
                return failed(&e.context(format!("started {} of {count} albums", started.len())));
            }
        }
    }
    let msg = match started.as_slice() {
        [one_title] => format!("Downloading {one_title}"),
        _ => format!("Downloading {count} albums"),
    };
    one(flash_ok(&msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slsk_engine::slsk_proto::peer::FileEntry;

    fn dir(name: &str, files: usize) -> Directory {
        Directory {
            name: RawStr::from(name.to_string()),
            files: (0..files)
                .map(|i| FileEntry {
                    name: RawStr::from(format!("{i}.flac")),
                    size: 1,
                    extension: "flac".into(),
                    attrs: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_level_lists_folders_below_it_including_unlisted_ones() {
        let dirs = vec![
            dir("@@music\\A\\One", 2),
            dir("@@music\\A\\Two", 3),
            dir("@@music\\B", 1),
            dir("@@other\\C\\D", 4),
        ];
        let (top, here) = level(&dirs, b"");
        assert!(here.is_none());
        assert_eq!(
            top.iter()
                .map(|c| (c.name.as_str(), c.files))
                .collect::<Vec<_>>(),
            vec![("@@music", 6), ("@@other", 4)]
        );
        let (music, _) = level(&dirs, b"@@music");
        assert_eq!(
            music.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        let (below_b, here) = level(&dirs, b"@@music\\B");
        assert!(below_b.is_empty());
        assert_eq!(here.map(|d| d.files.len()), Some(1));
        // A sibling whose name starts the same is not beneath it.
        let (a, _) = level(
            &[dir("@@music\\A", 1), dir("@@music\\AB\\x", 1)],
            b"@@music\\A",
        );
        assert!(a.is_empty());
    }

    #[test]
    fn a_tree_downloads_as_albums_with_discs_kept_together() {
        let dirs = vec![
            dir("@@music\\A\\One", 2),
            dir("@@music\\A\\Two\\CD1", 3),
            dir("@@music\\A\\Two\\CD2", 3),
            dir("@@music\\AB", 1),
        ];
        let a = albums(&dirs, b"@@music\\A");
        assert_eq!(
            a.iter()
                .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v.len()))
                .collect::<Vec<_>>(),
            vec![
                ("@@music\\A\\One".to_string(), 2),
                ("@@music\\A\\Two".to_string(), 6)
            ]
        );
        let two = &a[b"@@music\\A\\Two".as_slice()];
        assert_eq!(two[0].2, "CD1");
        assert_eq!(two[5].2, "CD2");
        // A disc folder opened directly is its own album.
        let cd = albums(&dirs, b"@@music\\A\\Two\\CD1");
        assert_eq!(cd.len(), 1);
        assert_eq!(cd.values().next().unwrap()[0].2, "");
        assert_eq!(albums(&dirs, b"").len(), 3);
    }

    fn place(at: &str, open: &[&str]) -> Place {
        Place {
            username: "u".into(),
            at: at.as_bytes().to_vec(),
            open: open.iter().map(|o| o.as_bytes().to_vec()).collect(),
            more: BTreeMap::new(),
        }
    }

    fn query(href: &str) -> BrowseQuery {
        Query::try_from_uri(&href.parse().unwrap()).unwrap().0
    }

    fn reread(href: &str) -> Place {
        Place::from_query(&query(href)).unwrap()
    }

    #[test]
    fn the_address_names_expanded_folders_relative_to_the_open_one() {
        let mut p = place("@@music\\A", &["@@music\\A\\One", "@@music\\A\\One\\CD1"]);
        p.more.insert(b"@@music\\A".to_vec(), 2);
        let q = query(&p.href());
        assert_eq!(q.open, vec!["One", "One/CD1"]);
        assert_eq!(q.more, vec![".", "."]);
        assert_eq!(Place::from_query(&q).unwrap(), p);
    }

    #[test]
    fn a_name_the_address_cannot_spell_goes_as_its_bytes() {
        let mut p = place("@@m", &["@@m\\a/b"]);
        p.open.insert(b"@@m\\Bj\xf6rk".to_vec());
        let q = query(&p.href());
        assert!(q.open.iter().all(|o| o.starts_with(RAW)));
        assert_eq!(Place::from_query(&q).unwrap(), p);
    }

    #[test]
    fn expanded_folders_share_one_budget_of_rows() {
        let page = crate::ui::PAGE as usize;
        let dirs: Vec<Directory> = (0..3)
            .flat_map(|a| (0..page).map(move |i| dir(&format!("{a}\\{i:05}"), 1)))
            .collect();
        let p = place("", &["0", "1", "2"]);
        let mut left = 3 + 2 * page;
        let html = tree(&p, &dirs, b"", false, &mut left);
        assert_eq!(left, 0);
        assert_eq!(html.matches("class=\"node dir\"").count(), 3 + 2 * page);
        assert!(html.contains("Collapse other folders"));
    }

    #[test]
    fn collapsing_a_folder_forgets_what_was_open_under_it() {
        let p = place("", &["A", "A\\B", "AB"]);
        let q = reread(&p.toggle(b"A"));
        assert_eq!(q, place("", &["AB"]));
        let q = reread(&p.toggle(b"C"));
        assert!(q.open.contains(b"C".as_slice()));
    }

    #[test]
    fn moving_keeps_what_is_open_below_and_the_way_back_down() {
        let p = place("A\\B", &["A\\B\\C", "A\\B\\C\\D"]);
        // Down: what was open below the new folder stays open.
        let down = reread(&p.go(b"A\\B\\C"));
        assert_eq!(down, place("A\\B\\C", &["A\\B\\C\\D"]));
        // Up: the folder left, and everything open in it, stays expanded.
        let up = reread(&p.go(b""));
        assert_eq!(up, place("", &["A", "A\\B", "A\\B\\C", "A\\B\\C\\D"]));
        // Sideways: nothing open elsewhere follows.
        let side = reread(&p.go(b"X"));
        assert_eq!(side, place("X", &[]));
    }

    #[test]
    fn a_long_folder_is_drawn_a_page_at_a_time() {
        let page = crate::ui::PAGE as usize;
        let dirs: Vec<Directory> = (0..page + 3)
            .map(|i| dir(&format!("@@m\\{i:05}"), 1))
            .collect();
        let p = place("@@m", &[]);
        let html = tree(&p, &dirs, b"@@m", false, &mut { MOST_ROWS });
        assert_eq!(html.matches("class=\"node dir\"").count(), page);
        assert!(html.contains("Show more"));
        let mut p = p;
        p.more.insert(b"@@m".to_vec(), 1);
        let html = tree(&p, &dirs, b"@@m", false, &mut { MOST_ROWS });
        assert_eq!(html.matches("class=\"node dir\"").count(), page + 3);
        assert!(!html.contains("Show more"));
    }

    #[test]
    fn crumbs_lead_back_up() {
        let c = crumbs(b"@@music\\A\\One");
        assert_eq!(
            c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["@@music", "A", "One"]
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&c[1].key).unwrap(),
            b"@@music\\A".to_vec()
        );
    }
}
