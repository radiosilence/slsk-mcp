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
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
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

#[derive(Clone, Default)]
pub(super) struct Cache(Arc<parking_lot::Mutex<Vec<(String, Instant, Listing)>>>);

const KEEP: usize = 4;
const FRESH: Duration = Duration::from_secs(600);

impl Cache {
    fn get(&self, username: &str) -> Option<Listing> {
        let mut c = self.0.lock();
        c.retain(|(_, at, _)| at.elapsed() < FRESH);
        c.iter()
            .find(|(u, ..)| u == username)
            .map(|(.., l)| l.clone())
    }

    fn put(&self, username: &str, listing: Listing) {
        let mut c = self.0.lock();
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
    fn from_form(form: &[(String, String)]) -> anyhow::Result<Self> {
        let username = super::field(form, "username")
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Say whose shares to browse."))?;
        let at = URL_SAFE_NO_PAD
            .decode(super::field(form, "key").unwrap_or_default())
            .map_err(|_| anyhow::anyhow!("not a folder key"))?;
        let paths = |name: &str| {
            form.iter()
                .filter(move |(k, _)| k == name)
                .map(|(_, v)| join(&at, v))
                .collect::<Vec<_>>()
        };
        let open = paths("open").into_iter().collect();
        let mut more = BTreeMap::new();
        for p in paths("more") {
            *more.entry(p).or_default() += 1;
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
        String::from_utf8_lossy(&path[start..]).replace('\\', "/")
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

/// A path named relative to `at`, back in the peer's own form.
fn join(at: &[u8], rel: &str) -> Vec<u8> {
    if rel == "." || rel.is_empty() {
        return at.to_vec();
    }
    let mut out = at.to_vec();
    if !out.is_empty() {
        out.push(b'\\');
    }
    out.extend_from_slice(rel.replace('/', "\\").as_bytes());
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

fn tree(p: &Place, dirs: &[Directory], at: &[u8], mine: bool) -> String {
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
    let shown = super::PAGE as usize * (1 + p.more.get(at).copied().unwrap_or(0));
    let total = children.len() + here.map_or(0, |d| d.files.len());
    let nodes = children
        .into_iter()
        .map(|c| {
            Node::Folder(FolderRow {
                go: p.go(&c.path),
                toggle: p.toggle(&c.path),
                inner: p
                    .open
                    .contains(&c.path)
                    .then(|| tree(p, dirs, &c.path, mine)),
                name: c.name,
                key: c.key,
                files: c.files,
            })
        })
        .chain(files)
        .take(shown)
        .collect();
    Nodes {
        username: &p.username,
        nodes,
        more: (total > shown).then(|| p.show_more(at)),
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
        rows: tree(p, dirs, &p.at, mine),
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

#[derive(serde::Deserialize)]
struct Fresh {
    #[serde(default)]
    fresh: bool,
}

/// Open a user's shares at a folder (the top when none is given). The
/// username arrives in a form field, never in the URL or an expression.
async fn browse(State(s): State<UiState>, Query(q): Query<Fresh>, body: Bytes) -> Response {
    browse_at(s, &fields(&body), q.fresh)
}

/// The same, from the address: `/browse?username=…&key=…`, which `browse`
/// reports as the place it drew.
async fn browse_get(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let fresh = super::field(&q, "fresh").is_some();
    browse_at(s, &q, fresh)
}

fn browse_at(s: UiState, form: &[(String, String)], fresh: bool) -> Response {
    let p = match Place::from_form(form) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let here = super::place(&p.href());
    let Some(engine) = s.app.session.engine().cloned() else {
        return failed(&anyhow::anyhow!("Sign in to Soulseek first."));
    };
    let cache = s.browsed.clone();
    if engine.username() == p.username {
        let listing = Arc::new(engine.own_shares());
        cache.put(&p.username, listing.clone());
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

    fn query(href: &str) -> Vec<(String, String)> {
        url::form_urlencoded::parse(href.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn the_address_names_expanded_folders_relative_to_the_open_one() {
        let mut p = place("@@music\\A", &["@@music\\A\\One", "@@music\\A\\One\\CD1"]);
        p.more.insert(b"@@music\\A".to_vec(), 2);
        let q = query(&p.href());
        let opens: Vec<&str> = q
            .iter()
            .filter(|(k, _)| k == "open")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(opens, vec!["One", "One/CD1"]);
        assert_eq!(q.iter().filter(|(k, v)| k == "more" && v == ".").count(), 2);
        assert_eq!(Place::from_form(&q).unwrap(), p);
    }

    #[test]
    fn collapsing_a_folder_forgets_what_was_open_under_it() {
        let p = place("", &["A", "A\\B", "AB"]);
        let q = Place::from_form(&query(&p.toggle(b"A"))).unwrap();
        assert_eq!(q, place("", &["AB"]));
        let q = Place::from_form(&query(&p.toggle(b"C"))).unwrap();
        assert!(q.open.contains(b"C".as_slice()));
    }

    #[test]
    fn moving_keeps_what_is_open_below_and_the_way_back_down() {
        let p = place("A\\B", &["A\\B\\C", "A\\B\\C\\D"]);
        // Down: what was open below the new folder stays open.
        let down = Place::from_form(&query(&p.go(b"A\\B\\C"))).unwrap();
        assert_eq!(down, place("A\\B\\C", &["A\\B\\C\\D"]));
        // Up: the folder left, and everything open in it, stays expanded.
        let up = Place::from_form(&query(&p.go(b""))).unwrap();
        assert_eq!(up, place("", &["A", "A\\B", "A\\B\\C", "A\\B\\C\\D"]));
        // Sideways: nothing open elsewhere follows.
        let side = Place::from_form(&query(&p.go(b"X"))).unwrap();
        assert_eq!(side, place("X", &[]));
    }

    #[test]
    fn a_long_folder_is_drawn_a_page_at_a_time() {
        let page = crate::ui::PAGE as usize;
        let dirs: Vec<Directory> = (0..page + 3)
            .map(|i| dir(&format!("@@m\\{i:05}"), 1))
            .collect();
        let p = place("@@m", &[]);
        let html = tree(&p, &dirs, b"@@m", false);
        assert_eq!(html.matches("class=\"node dir\"").count(), page);
        assert!(html.contains("Show more"));
        let mut p = p;
        p.more.insert(b"@@m".to_vec(), 1);
        let html = tree(&p, &dirs, b"@@m", false);
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
