//! Browsing a user's shares folder by folder, and downloading a folder or
//! files from them.
//!
//! A peer sends its whole share list at once, which for a large collection
//! is tens of thousands of folders and takes a while, so the last few lists
//! fetched are kept for a short time and each step through them is local.
//! Folders are named in URLs and forms by the base64 of their raw bytes:
//! peer paths are not always UTF-8, and a key is safe anywhere.

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

use super::{UiState, failed, fields, flash_ok, human, jobs_html, one, patch, sse};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/browse", post(browse))
        .route("/browse/files", post(download_files))
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

struct Child {
    key: String,
    name: String,
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
        })
        .collect();
    (children, here)
}

/// Each folder from the top down to `at`, for going back up.
fn crumbs(at: &[u8]) -> Vec<Child> {
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
            key: key(&path),
            name: RawStr(part.to_vec().into()).to_string_lossy(),
            files: 0,
        });
    }
    out
}

struct FileRow {
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

/// Folders shown at one level; a collection's top level can run to
/// thousands.
const SHOWN: usize = 500;

#[derive(Template)]
#[template(path = "browse.html")]
struct BrowseView {
    username: String,
    folders: usize,
    crumbs: Vec<Child>,
    children: Vec<Child>,
    more: usize,
    here_key: String,
    files: Vec<FileRow>,
    info: Option<Info>,
    error: Option<String>,
}

impl BrowseView {
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
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

fn render(username: &str, dirs: &[Directory], at: &[u8], info: Option<Info>) -> String {
    let (mut children, here) = level(dirs, at);
    let more = children.len().saturating_sub(SHOWN);
    children.truncate(SHOWN);
    let files = here
        .map(|d| {
            d.files
                .iter()
                .map(|f| {
                    let mut full = at.to_vec();
                    full.push(b'\\');
                    full.extend_from_slice(f.name.as_bytes());
                    FileRow {
                        key: key(&full),
                        name: f.name.to_string_lossy(),
                        size: f.size,
                        quality: quality(f),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    BrowseView {
        username: username.to_string(),
        folders: dirs.len(),
        crumbs: crumbs(at),
        children,
        more,
        here_key: key(at),
        files,
        info,
        error: None,
    }
    .render()
    .unwrap_or_default()
}

fn note(username: &str, error: String) -> String {
    BrowseView {
        username: username.to_string(),
        folders: 0,
        crumbs: Vec::new(),
        children: Vec::new(),
        more: 0,
        here_key: String::new(),
        files: Vec::new(),
        info: None,
        error: Some(error),
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
    let form = fields(&body);
    let Some(username) = super::field(&form, "username").map(|u| u.trim().to_string()) else {
        return failed(&anyhow::anyhow!("Say whose shares to browse."));
    };
    let Ok(at) = URL_SAFE_NO_PAD.decode(super::field(&form, "key").unwrap_or_default()) else {
        return failed(&anyhow::anyhow!("not a folder key"));
    };
    let Some(engine) = s.app.session.engine().cloned() else {
        return failed(&anyhow::anyhow!("Sign in to Soulseek first."));
    };
    let cache = s.browsed.clone();
    sse(async_stream::stream! {
        let listing = match cache.get(&username).filter(|_| !q.fresh) {
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
                        yield Ok(patch(&render(&username, &l, &at, info)));
                        return;
                    }
                    Err(e) => {
                        yield Ok(patch(&note(&username, format!("{username} did not send their shares: {e}"))));
                        return;
                    }
                }
            }
        };
        yield Ok(patch(&render(&username, &listing, &at, None)));
        yield Ok(patch("<div id=\"flash\"></div>"));
    })
    .into_response()
}

/// Download the ticked files of one folder as an album of their own. Sizes
/// come from the listing the page was drawn from, not from the form.
async fn download_files(State(s): State<UiState>, body: Bytes) -> Response {
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
    let Some(dir) = listing.iter().find(|d| d.name.as_bytes() == at.as_slice()) else {
        return failed(&anyhow::anyhow!("That folder is no longer in the listing."));
    };
    let wanted: Vec<Vec<u8>> = form
        .iter()
        .filter(|(k, _)| k == "f")
        .filter_map(|(_, v)| URL_SAFE_NO_PAD.decode(v).ok())
        .collect();
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
        Ok(job) => one(format!(
            "{}\n{}",
            jobs_html(&s.app).await,
            flash_ok(&format!("Downloading {}", job.title))
        )),
        Err(e) => failed(&e),
    }
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
