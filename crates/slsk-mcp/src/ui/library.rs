//! The library as a whole: finding albums, spare copies to bin, albums to
//! re-file under the current rules, and enriching tags in the background.
//!
//! Every action names albums by exact path (`path:=…`), so it touches only
//! what the person was shown, and goes through the same `Library` calls as
//! the GraphQL mutations.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, one, patch, sse};
use crate::App;
use crate::library::{AlbumMove, DuplicateSet, EnrichedAlbum, LibraryAlbum, Preview};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/library", get(view))
        .route("/library/find", post(find))
        .route("/library/enrich", post(enrich))
        .route("/library/preview", get(preview))
        .route("/library/refile", post(refile_selected))
        .route("/library/refile-safe", post(refile_safe))
        .route("/library/bin", post(bin))
        .route("/library/bin-all", post(bin_all))
}

/// Terms that match exactly the albums at `paths`, as alternatives.
fn by_path<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut query = Vec::new();
    for p in paths {
        if !query.is_empty() {
            query.push(",".to_string());
        }
        query.push(format!("path:={p}"));
    }
    query
}

/// What a re-file would change, from the album's directory and the one the
/// rules give it. The first three only correct how the folder is written;
/// the fourth moves the album somewhere its folder does not already say.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Change {
    FileNames,
    Spelling,
    Year,
    Tags,
    Refused,
}

impl Change {
    fn of(m: &AlbumMove) -> Self {
        let Some(to) = m.to.as_deref() else {
            return Self::Refused;
        };
        let spelling = |s: &str| s.replace('_', "-");
        if m.from == to {
            Self::FileNames
        } else if spelling(&m.from) == spelling(to) {
            Self::Spelling
        } else if without_years(&spelling(&m.from)) == without_years(&spelling(to)) {
            Self::Year
        } else {
            Self::Tags
        }
    }

    fn safe(self) -> bool {
        matches!(self, Self::FileNames | Self::Spelling | Self::Year)
    }

    fn label(self) -> &'static str {
        match self {
            Self::FileNames => "File names only",
            Self::Spelling => "Folder spelling",
            Self::Year => "Year in the folder name",
            Self::Tags => "Tags disagree with the folder",
            Self::Refused => "Left where they are",
        }
    }

    fn why(self) -> &'static str {
        match self {
            Self::FileNames => "The album stays in its folder; its files are renamed.",
            Self::Spelling => {
                "The folder name differs only in characters the rules now write as '-'."
            }
            Self::Year => "The folder's year differs from the year in the tags.",
            Self::Tags => {
                "The tags name a different artist or album than the folder does. Check each before moving it."
            }
            Self::Refused => {
                "Moving these would collide with something, or the tags cannot be filed."
            }
        }
    }
}

/// Every run of four digits replaced, so paths differing only in a year
/// compare equal.
fn without_years(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut digits = String::new();
    for c in s.chars().chain(std::iter::once('\0')) {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        if digits.len() == 4 {
            out.push('#');
        } else {
            out.push_str(&digits);
        }
        digits.clear();
        if c != '\0' {
            out.push(c);
        }
    }
    out
}

struct Group {
    change: Change,
    moves: Vec<AlbumMove>,
}

/// How many of a group's albums to list; the rest are counted.
const SHOWN: usize = 50;

#[derive(Template)]
#[template(path = "library.html")]
struct LibraryView {
    root: String,
    dups: Vec<DuplicateSet>,
    spares: usize,
    groups: Vec<Group>,
    safe: usize,
    error: Option<String>,
}

impl LibraryView {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn key(&self, p: &str) -> String {
        key(p)
    }
    fn shown<'a>(&self, g: &'a Group) -> &'a [AlbumMove] {
        &g.moves[..g.moves.len().min(SHOWN)]
    }
    fn hidden(&self, g: &Group) -> usize {
        g.moves.len().saturating_sub(SHOWN)
    }
    fn album(&self, a: &LibraryAlbum) -> String {
        describe(a)
    }
    /// A refusal's reason with the library's own prefix taken off its paths.
    fn reason(&self, why: &str) -> String {
        why.replace(&format!("{}/", self.root), "")
    }
}

/// An album's id in the page: the same for the same path in every render,
/// so a result lands on its own row whatever has moved around it.
pub(super) fn key(path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn rel<'a>(root: &str, p: &'a str) -> &'a str {
    p.strip_prefix(root)
        .map(|r| r.trim_start_matches('/'))
        .unwrap_or(p)
}

fn describe(a: &LibraryAlbum) -> String {
    format!(
        "{} · {} track{}",
        if a.format.is_empty() { "?" } else { &a.format },
        a.tracks,
        if a.tracks == 1 { "" } else { "s" }
    )
}

fn grouped(plan: Vec<AlbumMove>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for m in plan {
        let change = Change::of(&m);
        match groups.iter_mut().find(|g| g.change == change) {
            Some(g) => g.moves.push(m),
            None => groups.push(Group {
                change,
                moves: vec![m],
            }),
        }
    }
    groups.sort_by_key(|g| g.change);
    groups
}

async fn html(app: &App) -> String {
    let root = app.library.root().to_string_lossy().into_owned();
    let result = async {
        let dups = app.library.duplicates(&[], false).await?;
        let plan = app.library.refile(&[], false).await?;
        anyhow::Ok((dups, plan))
    }
    .await;
    let view = match result {
        Ok((dups, plan)) => {
            let groups = grouped(plan);
            LibraryView {
                spares: dups.iter().map(|d| d.spares.len()).sum(),
                safe: groups
                    .iter()
                    .filter(|g| g.change.safe())
                    .map(|g| g.moves.len())
                    .sum(),
                dups,
                groups,
                root,
                error: None,
            }
        }
        Err(e) => LibraryView {
            root,
            dups: Vec::new(),
            spares: 0,
            groups: Vec::new(),
            safe: 0,
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

/// The library's overview, and the albums `q` finds when the address has a
/// search: what `find` showed, back after a reload.
async fn view(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let find = super::field(&q, "q").map(str::to_string);
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="library"><p class="note">Reading the library…</p></div>"#));
        if let Some(find) = find {
            yield Ok(patch(&found_html(&s, find).await));
        }
        yield Ok(patch(&html(&s.app).await));
    })
    .into_response()
}

#[derive(Template)]
#[template(path = "library_found.html")]
struct FoundView {
    q: String,
    albums: Vec<LibraryAlbum>,
    total: usize,
    root: String,
    error: Option<String>,
}

impl FoundView {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn album(&self, a: &LibraryAlbum) -> String {
        describe(a)
    }
}

#[derive(serde::Deserialize)]
struct FindForm {
    q: String,
}

async fn find(State(s): State<UiState>, axum::Form(f): axum::Form<FindForm>) -> Response {
    let q = f.q.trim().to_string();
    super::many(vec![
        patch(&found_html(&s, q.clone()).await),
        super::place("library", &super::place_url("/library", &[("q", &q)])),
    ])
}

async fn found_html(s: &UiState, q: String) -> String {
    let terms: Vec<String> = q.split_whitespace().map(String::from).collect();
    let root = s.app.library.root().to_string_lossy().into_owned();
    let view = match s.app.library.albums(&terms).await {
        Ok(albums) => FoundView {
            total: albums.len(),
            albums: albums.into_iter().take(200).collect(),
            q,
            root,
            error: None,
        },
        Err(e) => FoundView {
            q,
            albums: Vec::new(),
            total: 0,
            root,
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

/// A background enrichment and how far it has got. Enriching reads every
/// file and asks two services per album, seconds each, so it runs apart
/// from the request that started it.
#[derive(Default)]
pub(super) struct EnrichRun {
    asked: String,
    total: usize,
    done: usize,
    current: Option<String>,
    running: bool,
    results: Vec<EnrichedAlbum>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "enrich.html")]
struct EnrichView<'a> {
    run: &'a EnrichRun,
    root: String,
}

impl EnrichView<'_> {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn gain(&self, r: &EnrichedAlbum) -> String {
        r.gain_db
            .map_or_else(|| "no gain".into(), |g| format!("{g:+.1} dB"))
    }
    fn recent(&self) -> impl Iterator<Item = &EnrichedAlbum> {
        self.run.results.iter().rev().take(30)
    }
}

pub(super) fn enrich_html(s: &UiState) -> String {
    let run = s.enrich.lock();
    EnrichView {
        run: &run,
        root: s.app.library.root().to_string_lossy().into_owned(),
    }
    .render()
    .unwrap_or_default()
}

async fn enrich(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let (query, asked) = if let Some(p) = super::field(&form, "path") {
        (
            by_path([p]),
            rel(&s.app.library.root().to_string_lossy(), p).to_string(),
        )
    } else if let Some(a) = super::field(&form, "artist") {
        (vec![format!("albumartist:={a}")], a.to_string())
    } else if let Some(q) = super::field(&form, "q") {
        (
            q.split_whitespace().map(String::from).collect(),
            q.to_string(),
        )
    } else {
        return failed(&anyhow::anyhow!(
            "Say which albums; the whole library is enriched over the API, deliberately."
        ));
    };
    {
        let mut run = s.enrich.lock();
        if run.running {
            return failed(&anyhow::anyhow!(
                "Already enriching {}; start another when it finishes.",
                run.asked
            ));
        }
        *run = EnrichRun {
            asked,
            running: true,
            ..Default::default()
        };
    }
    let (app, state) = (s.app.clone(), s.enrich.clone());
    tokio::spawn(async move {
        let albums = match app.library.albums(&query).await {
            Ok(a) => a,
            Err(e) => {
                let mut run = state.lock();
                run.error = Some(format!("{e:#}"));
                run.running = false;
                return;
            }
        };
        state.lock().total = albums.len();
        for a in albums {
            state.lock().current = Some(a.path.clone());
            let result = app.library.enrich(&by_path([a.path.as_str()])).await;
            let mut run = state.lock();
            match result {
                Ok(r) => run.results.extend(r),
                Err(e) => run.results.push(EnrichedAlbum {
                    path: a.path,
                    gain_db: None,
                    genres: Vec::new(),
                    lyrics: 0,
                    problems: vec![format!("{e:#}")],
                }),
            }
            run.done += 1;
        }
        let mut run = state.lock();
        run.current = None;
        run.running = false;
    });
    one(format!("{}\n<div id=\"flash\"></div>", enrich_html(&s)))
}

#[derive(Template)]
#[template(path = "library_preview.html")]
struct PreviewView {
    key: String,
    root: String,
    /// Into one folder, the files' names only; otherwise whole paths.
    into: Option<String>,
    moves: Vec<(String, String)>,
    note: Option<String>,
}

/// Every file re-filing one album would move, worked out now.
async fn preview(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let Some(path) = super::field(&q, "path") else {
        return failed(&anyhow::anyhow!("no album given"));
    };
    let root = s.app.library.root().to_string_lossy().into_owned();
    let mut view = PreviewView {
        key: key(path),
        root,
        into: None,
        moves: Vec::new(),
        note: None,
    };
    match s.app.library.preview(&by_path([path])).await {
        Ok(Some(Preview::Moves(moves))) => {
            let dirs: std::collections::HashSet<_> =
                moves.iter().map(|(_, to)| to.parent()).collect();
            let one_dir = dirs.len() == 1;
            if one_dir {
                view.into = moves
                    .first()
                    .and_then(|(_, to)| to.parent())
                    .map(|d| rel(&view.root, &d.to_string_lossy()).to_string());
            }
            let name = |p: &std::path::Path| {
                if one_dir {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                } else {
                    rel(&view.root, &p.to_string_lossy()).to_string()
                }
            };
            view.moves = moves
                .iter()
                .map(|(from, to)| (name(from), name(to)))
                .collect();
        }
        Ok(Some(Preview::Refused(why))) => {
            view.note = Some(format!(
                "Left where it is: {}",
                why.replace(&format!("{}/", view.root), "")
            ))
        }
        Ok(Some(Preview::InPlace)) => view.note = Some("Already where the rules file it.".into()),
        Ok(None) => view.note = Some("No longer in the library.".into()),
        Err(e) => view.note = Some(format!("{e:#}")),
    }
    one(view.render().unwrap_or_default())
}

#[derive(Template)]
#[template(path = "library_row.html")]
struct RowView {
    key: String,
    from: String,
    /// What happened: `moved`, `left` or `failed`.
    outcome: &'static str,
    detail: String,
}

/// Re-file the albums ticked on the page, one at a time, each row showing
/// its outcome as it lands. Batches started while one runs queue behind it
/// for the library; the page is drawn again when the last one finishes.
async fn refile_selected(State(s): State<UiState>, body: Bytes) -> Response {
    use std::sync::atomic::Ordering;
    let paths: Vec<String> = fields(&body)
        .into_iter()
        .filter(|(k, v)| k == "path" && !v.is_empty())
        .map(|(_, v)| v)
        .collect();
    if paths.is_empty() {
        return failed(&anyhow::anyhow!("nothing selected"));
    }
    let root = s.app.library.root().to_string_lossy().into_owned();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AlbumMove>();
    s.refiling.fetch_add(1, Ordering::SeqCst);
    let (app, query) = (s.app.clone(), by_path(paths.iter().map(String::as_str)));
    let work = tokio::spawn(async move {
        app.library
            .refile_each(&query, |m| {
                let _ = tx.send(m);
            })
            .await
    });
    let (app, refiling) = (s.app.clone(), s.refiling.clone());
    sse(async_stream::stream! {
        for p in &paths {
            yield Ok(patch(&format!(r#"<span id="st-{}" class="pill">queued</span>"#, key(p))));
        }
        let mut moved = 0;
        while let Some(m) = rx.recv().await {
            let row = RowView {
                key: key(&m.from),
                from: rel(&root, &m.from).to_string(),
                outcome: match (&m.to, &m.refused) {
                    (Some(_), _) => { moved += 1; "moved" }
                    (None, _) => "left",
                },
                detail: match (&m.to, &m.refused) {
                    (Some(to), _) => format!("→ {}", rel(&root, to)),
                    (None, Some(why)) => why.replace(&format!("{root}/"), ""),
                    (None, None) => "Already where the rules file it.".into(),
                },
            };
            yield Ok(patch(&row.render().unwrap_or_default()));
        }
        let result = work.await.map_err(anyhow::Error::from).and_then(|r| r);
        let last = refiling.fetch_sub(1, Ordering::SeqCst) == 1;
        let flash = match &result {
            Ok(()) => flash_ok(&format!("Re-filed {moved} of {}", paths.len())),
            Err(e) => super::flash_err(&format!("Stopped after {moved}: {e:#}")),
        };
        if last {
            yield Ok(patch(&html(&app).await));
        }
        yield Ok(patch(&flash));
    })
    .into_response()
}

/// Re-file every album whose change only corrects how its folder is
/// written. The plan is taken again here and filtered by the same rule, so
/// an album whose tags changed since the page was drawn is left alone.
async fn refile_safe(State(s): State<UiState>) -> Response {
    let result = async {
        let plan = s.app.library.refile(&[], false).await?;
        let safe: Vec<&str> = plan
            .iter()
            .filter(|m| Change::of(m).safe())
            .map(|m| m.from.as_str())
            .collect();
        if safe.is_empty() {
            return anyhow::Ok(0);
        }
        Ok(s.app.library.refile(&by_path(safe), true).await?.len())
    }
    .await;
    match result {
        Ok(n) => one(format!(
            "{}\n{}",
            html(&s.app).await,
            flash_ok(&format!("Re-filed {n} albums"))
        )),
        Err(e) => failed(&e),
    }
}

async fn bin(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let paths: Vec<&str> = form
        .iter()
        .filter(|(k, v)| k == "path" && !v.is_empty())
        .map(|(_, v)| v.as_str())
        .collect();
    if paths.len() < 2 {
        return failed(&anyhow::anyhow!("no duplicate set given"));
    }
    binned(&s, s.app.library.duplicates(&by_path(paths), true).await).await
}

async fn bin_all(State(s): State<UiState>) -> Response {
    binned(&s, s.app.library.duplicates(&[], true).await).await
}

async fn binned(s: &UiState, result: anyhow::Result<Vec<DuplicateSet>>) -> Response {
    match result {
        Ok(sets) => {
            let n: usize = sets.iter().map(|d| d.spares.len()).sum();
            let msg = if n == 0 {
                "Nothing binned: those copies are no longer duplicates.".to_string()
            } else {
                format!("Moved {n} spare copies to the bin")
            };
            one(format!("{}\n{}", html(&s.app).await, flash_ok(&msg)))
        }
        Err(e) => failed(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(from: &str, to: Option<&str>) -> Change {
        Change::of(&AlbumMove {
            from: from.into(),
            to: to.map(Into::into),
            refused: to.is_none().then(|| "collides".into()),
            files: 1,
        })
    }

    #[test]
    fn a_move_is_named_for_the_least_it_changes() {
        let from = "/m/Burial/(2007) Untrue [FLAC]";
        assert_eq!(change(from, Some(from)), Change::FileNames);
        assert_eq!(
            change(
                "/m/Burial/(2007) Untrue_ Remixes [FLAC]",
                Some("/m/Burial/(2007) Untrue- Remixes [FLAC]")
            ),
            Change::Spelling
        );
        assert_eq!(
            change(from, Some("/m/Burial/(2008) Untrue [FLAC]")),
            Change::Year
        );
        assert_eq!(
            change(
                "/m/Burial_/(2007) Untrue [FLAC]",
                Some("/m/Burial-/(2008) Untrue [FLAC]")
            ),
            Change::Year
        );
        assert_eq!(
            change(from, Some("/m/Kode9 & Burial/(2007) Untrue [FLAC]")),
            Change::Tags
        );
        assert_eq!(change(from, None), Change::Refused);
    }

    #[test]
    fn only_four_digit_runs_count_as_years() {
        assert_eq!(without_years("(1999) 101 Hits 12345"), "(#) 101 Hits 12345");
        assert_ne!(
            without_years("/m/A/(2001) X"),
            without_years("/m/A/(2001) X 2")
        );
    }

    #[test]
    fn paths_become_exact_alternatives() {
        assert_eq!(
            by_path(["/m/a", "/m/b"]),
            vec!["path:=/m/a", ",", "path:=/m/b"]
        );
        let q = sift::library::Query::parse(&by_path(["/m/a, b+", "/m/-c"])).unwrap();
        assert_eq!(q.sort.len(), 0);
    }
}
