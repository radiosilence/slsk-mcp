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

use koan_kit_web as kit;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use askama_datastar::Html;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::slsk_proto::peer::Directory;

use super::{Body, Page, Tab, View};
use crate::web::{Draw, UiState, failed, field, human, sent};

pub(in crate::web) type Listing = Arc<Vec<Directory>>;

/// This client's own listing, read from its share index. It is browsed
/// but never downloaded from, so it neither takes a peer's slot nor answers
/// a download.
#[derive(Clone, Default)]
pub(in crate::web) struct Own(Arc<parking_lot::Mutex<Option<(Instant, Listing)>>>);

/// Imports change the library often, so its own listing is read again
/// sooner than a peer's; between reads each step through it is a lookup,
/// not a copy of the share index.
const OWN_FRESH: Duration = Duration::from_secs(60);

impl Own {
    pub(in crate::web) fn listing(&self, engine: &slsk_engine::Engine, fresh: bool) -> Listing {
        let mut c = self.0.lock();
        match &*c {
            Some((at, l)) if !fresh && at.elapsed() < OWN_FRESH => l.clone(),
            _ => {
                let l: Listing = Arc::new(engine.own_shares());
                *c = Some((Instant::now(), l.clone()));
                l
            }
        }
    }
}

pub(in crate::web) fn key(raw: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(raw)
}

pub(in crate::web) struct Child {
    path: Vec<u8>,
    pub(in crate::web) key: String,
    pub(in crate::web) name: String,
    /// Files in it and every folder under it.
    pub(in crate::web) files: usize,
}

/// The folders directly under `at` (the top level when empty), and `at`
/// itself when the peer listed it. Peers list only the folders holding
/// files, so intermediate folders are inferred from the paths below them.
pub(in crate::web) fn level<'a>(
    dirs: &'a [Directory],
    at: &[u8],
) -> (Vec<Child>, Option<&'a Directory>) {
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
pub(in crate::web) fn crumbs(at: &[u8]) -> Vec<Child> {
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

pub(in crate::web) struct FileRow {
    key: String,
    name: String,
    size: u64,
    quality: String,
}

#[derive(Clone)]
pub(in crate::web) struct Info {
    pub(in crate::web) description: String,
    pub(in crate::web) uploads: u32,
    pub(in crate::web) queue: u32,
    pub(in crate::web) free: bool,
}

/// Where a user's tree is: the open folder and the folders expanded under
/// it. All of it is in the address, so a reload, a link, or back and
/// forward draw the same tree. Expanded folders are named relative to the
/// open one (`open=A&open=A/B`), which keeps the address readable and lets
/// it survive a move up or down.
#[derive(Clone, Debug, Default, PartialEq)]
pub(in crate::web) struct Place {
    pub(in crate::web) username: String,
    pub(in crate::web) at: Vec<u8>,
    pub(in crate::web) open: BTreeSet<Vec<u8>>,
}

/// `path` lies below `dir`, at any depth.
fn under(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return !path.is_empty();
    }
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'\\'
}

/// `path` is `dir` or lies below it.
fn within(path: &[u8], dir: &[u8]) -> bool {
    path == dir || under(path, dir)
}

impl Place {
    pub(in crate::web) fn from_query(q: &BrowseQuery) -> anyhow::Result<Self> {
        let username = q.username.trim().to_string();
        if username.is_empty() {
            anyhow::bail!("Say whose shares to browse.");
        }
        let at = URL_SAFE_NO_PAD
            .decode(&q.key)
            .map_err(|_| anyhow::anyhow!("not a folder key"))?;
        let open = q.open.iter().map(|r| join(&at, r)).collect();
        Ok(Self { username, at, open })
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

    pub(in crate::web) fn href(&self) -> String {
        let key = key(&self.at);
        let mut params = vec![("username", self.username.clone()), ("key", key)];
        params.extend(self.open.iter().map(|p| ("open", self.rel(p))));
        let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        crate::web::place_url("/browse", &params)
    }

    /// The same tree with `path` expanded, or collapsed along with every
    /// folder under it.
    pub(in crate::web) fn toggle(&self, path: &[u8]) -> String {
        let mut p = self.clone();
        if !p.open.remove(path) {
            p.open.insert(path.to_vec());
        } else {
            p.open.retain(|o| !under(o, path));
        }
        p.href()
    }

    /// `target` as the open folder. What was expanded below it stays
    /// expanded, and going up leaves the way back down expanded, as Finder
    /// does.
    pub(in crate::web) fn go(&self, target: &[u8]) -> String {
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
        }
        .href()
    }
}

/// Marks a relative path the address cannot spell as itself: one that is
/// not UTF-8 or holds a `/`, given instead as the base64 of its bytes.
pub(in crate::web) const RAW: &str = "~";

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

pub(in crate::web) struct FolderRow {
    name: String,
    key: String,
    /// Files in it and every folder under it.
    files: usize,
    go: String,
    toggle: String,
    /// Its own rows, drawn when it is expanded.
    inner: Option<Html>,
}

pub(in crate::web) enum Node {
    Folder(FolderRow),
    File(FileRow),
}

/// The rows of one folder in the tree: its folders, then its files, all of
/// them, since the peer sent the whole listing at once. Folders expanded in
/// the address are drawn with their own rows beneath them.
#[derive(Template)]
#[template(path = "browse/nodes.html")]
struct Nodes {
    username: String,
    nodes: Vec<Node>,
    /// They are the open folder's own, whose files can be ticked.
    top: bool,
    /// This client's own shares, which there is nothing to download from.
    mine: bool,
}

impl Nodes {
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
}

pub(in crate::web) fn tree(p: &Place, dirs: &[Directory], at: &[u8], mine: bool) -> Html {
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
    let mut nodes: Vec<Node> = children
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
        .collect();
    nodes.extend(files);
    Nodes {
        username: p.username.clone(),
        nodes,
        top: at == p.at.as_slice(),
        mine,
    }
    .draw()
}

struct Crumb {
    name: String,
    href: String,
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

/// A user's shares at the place the address names: the tree, or why it is not here yet.
#[derive(Template)]
#[template(path = "browse/shares.html")]
pub(in crate::web) struct Shares {
    username: String,
    mine: bool,
    banned: bool,
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
    rows: Option<Html>,
    info: Option<Info>,
    /// Why there is no tree: the peer's answer is on its way, or did not come.
    note: Option<String>,
    /// The note says the peer's answer is on its way.
    asking: bool,
}

impl Shares {
    pub(in crate::web) fn tree(
        p: &Place,
        dirs: &[Directory],
        info: Option<Info>,
        mine: bool,
        banned: bool,
    ) -> Self {
        let mut crumbs = vec![Crumb {
            name: "top".into(),
            href: p.go(b""),
        }];
        crumbs.extend(self::crumbs(&p.at).into_iter().map(|c| Crumb {
            href: p.go(&c.path),
            name: c.name,
        }));
        let up = (crumbs.len() > 1).then(|| crumbs[crumbs.len() - 2].href.clone());
        Self {
            username: p.username.clone(),
            mine,
            banned,
            folders: dirs.len(),
            crumbs,
            up,
            refresh: format!("{}&fresh=1", p.href()),
            here_key: key(&p.at),
            tree_files: dirs
                .iter()
                .filter(|d| within(d.name.as_bytes(), &p.at))
                .map(|d| d.files.len())
                .sum(),
            has_files: dirs
                .iter()
                .any(|d| d.name.as_bytes() == p.at.as_slice() && !d.files.is_empty()),
            rows: Some(tree(p, dirs, &p.at, mine)),
            info,
            note: None,
            asking: false,
        }
    }

    /// The shares at `p` as they stand: this account's own, a peer's answer
    /// or refusal, or a note that the peer is being asked.
    pub(in crate::web) fn of(s: &UiState, engine: &slsk_engine::Engine, p: &Place) -> Self {
        if engine.username() == p.username {
            return Self::tree(p, &s.own.listing(engine, false), None, true, false);
        }
        match s.peers.shares.answer(&p.username) {
            Some(Ok((listing, info))) => {
                Self::tree(p, &listing, info, false, engine.is_banned(&p.username))
            }
            Some(Err(e)) => Self::note(
                &p.username,
                format!("{} did not send their shares: {e}", p.username),
                false,
            ),
            None => Self::note(
                &p.username,
                format!("Asking {} for their shares…", p.username),
                true,
            ),
        }
    }

    fn note(username: &str, note: String, asking: bool) -> Self {
        Self {
            username: username.to_string(),
            mine: false,
            banned: false,
            folders: 0,
            crumbs: Vec::new(),
            up: None,
            refresh: String::new(),
            here_key: String::new(),
            tree_files: 0,
            has_files: false,
            rows: None,
            info: None,
            note: Some(note),
            asking,
        }
    }
}

pub(in crate::web) struct Browse;

#[derive(Template)]
#[template(path = "browse/page.html")]
struct BrowsePage<'a> {
    username: &'a str,
    /// The account signed in, whose own shares can be browsed too.
    me: Option<String>,
    shares: Option<Html>,
}

/// A tree's place as the address names it: see [`Place`]. `fresh` asks the
/// peer for their shares again rather than using the listing kept.
#[derive(serde::Deserialize, Default)]
pub(in crate::web) struct BrowseQuery {
    #[serde(default)]
    pub(in crate::web) username: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    pub(in crate::web) open: Vec<String>,
    #[serde(default)]
    fresh: Option<String>,
}

impl Page for Browse {
    const TAB: Tab = Tab::Browse;
    const STREAM: &'static str = "/stream/browse";
    type Args = View<BrowseQuery>;

    async fn body(s: &UiState, View(q): View<BrowseQuery>) -> anyhow::Result<Body> {
        let engine = s.app.session.engine();
        let me = engine.map(|e| e.username());
        let username = q.username.trim();
        if username.is_empty() {
            return Ok(Body::of(&BrowsePage {
                username,
                me,
                shares: None,
            }));
        }
        let p = Place::from_query(&q)?;
        let Some(engine) = engine else {
            return Ok(Body::of(&BrowsePage {
                username,
                me,
                shares: None,
            }));
        };
        let mine = engine.username() == p.username;
        // Asked to look again: the listing is forgotten, and the page moves to
        // its address, so a reload does not ask again.
        if q.fresh.is_some() {
            if mine {
                s.own.listing(engine, true);
            } else {
                s.peers.shares.forget(&p.username);
            }
            return Ok(Body::redirect(p.href()));
        }
        if !mine {
            s.peers.ask_shares(engine, &p.username);
        }
        let shares = Shares::of(s, engine, &p);
        Ok(Body::of(&BrowsePage {
            username,
            me,
            shares: Some(shares.draw()),
        })
        .heading(Some(p.username.clone())))
    }
}

/// Download the ticked files of one folder as an album of their own. Sizes
/// come from the listing the page was drawn from, not from the form.
pub(in crate::web) async fn download_files(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let wanted: Vec<Vec<u8>> = form
        .iter()
        .filter(|(k, _)| k == "f")
        .filter_map(|(_, v)| URL_SAFE_NO_PAD.decode(v).ok())
        .collect();
    // A file downloaded from its own row names no folder: it is the file's.
    let at = match field(&form, "key") {
        Some(k) => URL_SAFE_NO_PAD.decode(k).ok(),
        None => wanted.first().map(|f| parent(f).to_vec()),
    };
    let (Some(username), Some(at)) = (field(&form, "username"), at) else {
        return failed(&anyhow::anyhow!("no folder given"));
    };
    let Some(Ok((listing, _))) = s.peers.shares.answer(username) else {
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
        Ok(job) => {
            askama_datastar::events([crate::web::flash::ok(format!("Downloading {}", job.title))
                .patch()
                .into()])
        }
        Err(e) => failed(&e),
    }
}

/// A folder's download: everything under it, one job per album. A folder of
/// more than a hundred files asks first.
pub(in crate::web) fn tree_download<'a>(
    button: kit::Button<'a>,
    username: &str,
    key: &str,
    files: &usize,
) -> kit::Button<'a> {
    let button = button.post(crate::web::place_url(
        "/browse/tree",
        &[("username", username), ("key", key)],
    ));
    if *files > 100 {
        button
            .confirm("Download everything under this folder?", "Download")
            .detail(format!("It is {files} files, one job per album."))
    } else {
        button
    }
}

/// A file's download, as an album of its own.
pub(in crate::web) fn file_download(username: &str, key: &str) -> String {
    crate::web::place_url("/browse/files", &[("username", username), ("f", key)])
}

/// The folder a file or folder is in; the top for one at the top.
fn parent(path: &[u8]) -> &[u8] {
    path.iter()
        .rposition(|b| *b == b'\\')
        .map_or(&[][..], |i| &path[..i])
}

/// Every folder at or under `at` that holds music, grouped into albums: a
/// disc folder ("CD1") belongs to its parent, and its name becomes the
/// file's subdirectory so same-named tracks on different discs stay apart.
/// A folder with no music (scans, artwork, extras) goes with the nearest
/// album above it, as a subdirectory, and is left out when there is none:
/// alone it is no album, and searching for its name finds someone else's.
pub(in crate::web) fn albums(
    dirs: &[Directory],
    at: &[u8],
) -> BTreeMap<Vec<u8>, Vec<(RawStr, u64, String)>> {
    let mut out: BTreeMap<Vec<u8>, Vec<(RawStr, u64, String)>> = BTreeMap::new();
    let mut extras = Vec::new();
    for d in dirs {
        let name = d.name.as_bytes();
        if d.files.is_empty() || !within(name, at) {
            continue;
        }
        let music = d
            .files
            .iter()
            .any(|f| crate::folders::is_audio_name(&f.name.to_string_lossy()));
        if !music {
            extras.push(d);
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
    for d in extras {
        let name = d.name.as_bytes();
        let Some(root) = out
            .keys()
            .filter(|r| under(name, r))
            .max_by_key(|r| r.len())
            .cloned()
        else {
            continue;
        };
        let sub = String::from_utf8_lossy(&name[root.len() + 1..]).replace('\\', "/");
        let files = out.get_mut(&root).expect("a key just found");
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
pub(in crate::web) async fn download_tree(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let (Some(username), Some(at)) = (
        field(&form, "username"),
        field(&form, "key").and_then(|k| URL_SAFE_NO_PAD.decode(k).ok()),
    ) else {
        return failed(&anyhow::anyhow!("no folder given"));
    };
    let Some(Ok((listing, _))) = s.peers.shares.answer(username) else {
        return failed(&anyhow::anyhow!(
            "That listing has expired; browse the folder again."
        ));
    };
    let albums = albums(&listing, &at);
    if albums.is_empty() {
        return failed(&anyhow::anyhow!("There is no music under that folder."));
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
        let folder = RawStr(root.into()).to_string_lossy();
        let title = folder.rsplit('\\').next().unwrap_or_default().to_string();
        match s
            .app
            .jobs
            .from_listing(username, Some(folder), files, title)
            .await
        {
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
    askama_datastar::events([
        crate::web::flash::ok(&msg).patch().into(),
        kit::Confirm::close().into(),
    ])
}
