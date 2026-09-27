//! The library as a whole: finding albums, spare copies to bin, albums to
//! re-file under the current rules, and enriching tags in the background.
//!
//! Every action names albums by exact path (`path:=…`), so it touches only
//! what the person was shown, and goes through the same `Library` calls as
//! the GraphQL mutations.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, one, patch, sse};
use crate::App;
use crate::library::{AlbumMove, DuplicateSet, EnrichedAlbum, LibraryAlbum};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/library", get(view))
        .route("/library/find", post(find))
        .route("/library/enrich", post(enrich))
        .route("/library/refile", post(refile_one))
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

/// Reading the whole library takes a moment on a large one, so say so first.
async fn view(State(s): State<UiState>) -> Response {
    let app = s.app.clone();
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="library"><p class="note">Reading the library…</p></div>"#));
        yield Ok(patch(&html(&app).await));
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
    one(view.render().unwrap_or_default())
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

async fn refile_one(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(path) = super::field(&form, "path") else {
        return failed(&anyhow::anyhow!("no album given"));
    };
    let query = by_path([path]);
    let result = async {
        let plan = s.app.library.refile(&query, false).await?;
        match plan.first() {
            None => anyhow::bail!("That album is already where the rules put it."),
            Some(AlbumMove {
                refused: Some(why), ..
            }) => anyhow::bail!("Left where it is: {why}"),
            Some(_) => {}
        }
        s.app.library.refile(&query, true).await
    }
    .await;
    match result {
        Ok(moved) => {
            let root = s.app.library.root().to_string_lossy().into_owned();
            let to = moved
                .first()
                .and_then(|m| m.to.as_deref())
                .map(|t| rel(&root, t).to_string())
                .unwrap_or_default();
            one(format!(
                "{}\n{}",
                html(&s.app).await,
                flash_ok(&format!("Moved to {to}"))
            ))
        }
        Err(e) => failed(&e),
    }
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
