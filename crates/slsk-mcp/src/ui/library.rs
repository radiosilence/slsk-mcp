//! The library as one list of albums, newest import first, narrowed by a
//! search, an import date, or what needs attention (spare copies, albums
//! not where the rules file them). Albums ticked in it are acted on
//! together: every action is previewed album by album first, and applying
//! applies what the preview showed, in the background, reporting each album.
//!
//! Every action names albums by exact path (`path:=…`), so it touches only
//! what the person was shown, and goes through the same `Library` calls as
//! the GraphQL mutations.

use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, patch, sse};
use crate::library::{AlbumMove, CreditWrite, LibraryAlbum, Overview, Preview};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/library", get(view))
        .route("/library/find", post(find))
        .route("/library/preview/{action}", post(preview))
        .route("/library/apply/{action}", post(apply))
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

    fn label(self) -> &'static str {
        match self {
            Self::FileNames => "file names only",
            Self::Spelling => "folder spelling",
            Self::Year => "year in the folder name",
            Self::Tags => "tags disagree with the folder",
            Self::Refused => "cannot be re-filed",
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

/// When an album was imported, as a person would say it.
fn imported(secs: Option<i64>) -> String {
    let Some(t) = secs.and_then(|s| chrono::DateTime::from_timestamp(s, 0)) else {
        return String::new();
    };
    let days = (chrono::Utc::now() - t).num_days();
    match days {
        ..1 => super::ago(t),
        1..7 => format!("{days}d ago"),
        _ => t.format("%-d %b %Y").to_string(),
    }
}

/// What the list shows: a search, an import date, and which albums.
#[derive(Clone, Default, Debug, PartialEq)]
struct Filter {
    q: String,
    /// `today`, `week`, `month`, or `date` with `from` (`YYYY-MM-DD`).
    since: String,
    from: String,
    /// Empty for every album, `dups` or `moves`.
    show: String,
    page: usize,
}

/// Albums listed at once; the rest are a page away.
const PAGE: usize = 100;

impl Filter {
    fn from_fields(f: &[(String, String)]) -> Self {
        let get = |k| super::field(f, k).unwrap_or_default().trim().to_string();
        Self {
            q: get("q"),
            since: get("since"),
            from: get("from"),
            show: get("show"),
            page: get("page").parse().unwrap_or(0),
        }
    }

    /// The earliest import time the filter admits, in seconds.
    fn since_secs(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let day = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp());
        match self.since.as_str() {
            "today" => day(now.date_naive()),
            "week" => Some((now - chrono::Duration::days(7)).timestamp()),
            "month" => Some((now - chrono::Duration::days(30)).timestamp()),
            "date" => chrono::NaiveDate::parse_from_str(&self.from, "%Y-%m-%d")
                .ok()
                .and_then(day),
            _ => None,
        }
    }

    /// The query: the search's terms, the import date, then newest first.
    /// A sort in the search comes before the default one.
    fn terms(&self) -> Vec<String> {
        let mut terms: Vec<String> = self.q.split_whitespace().map(String::from).collect();
        if let Some(t) = self.since_secs(chrono::Utc::now()) {
            terms.push(format!("added:{t}.."));
        }
        terms.extend(["added-", "albumartist+", "album+"].map(String::from));
        terms
    }

    fn url(&self) -> String {
        let page = if self.page > 0 {
            self.page.to_string()
        } else {
            String::new()
        };
        let from = if self.since == "date" {
            self.from.as_str()
        } else {
            ""
        };
        super::place_url(
            "/library",
            &[
                ("q", &self.q),
                ("since", &self.since),
                ("from", from),
                ("show", &self.show),
                ("page", &page),
            ],
        )
    }

    fn at_page(&self, page: usize) -> Self {
        Self {
            page,
            ..self.clone()
        }
    }
}

/// What the whole library says about one album beyond its tags.
#[derive(Clone)]
struct Marks {
    /// Held more than once: the copy kept, and for a spare, why it is one.
    dup: Option<Dup>,
    /// Not where the rules file it: what would change, and where it would go.
    refile: Option<(Change, Option<String>)>,
}

#[derive(Clone)]
struct Dup {
    keep: String,
    /// Why this copy is spare; none for the copy kept.
    spare: Option<String>,
}

fn marks(o: &Overview) -> HashMap<String, Marks> {
    fn at<'a>(out: &'a mut HashMap<String, Marks>, p: &str) -> &'a mut Marks {
        out.entry(p.to_string()).or_insert(Marks {
            dup: None,
            refile: None,
        })
    }
    let mut out = HashMap::new();
    for d in &o.duplicates {
        at(&mut out, &d.keep.path).dup = Some(Dup {
            keep: d.keep.path.clone(),
            spare: None,
        });
        for s in &d.spares {
            at(&mut out, &s.album.path).dup = Some(Dup {
                keep: d.keep.path.clone(),
                spare: Some(s.reason.clone()),
            });
        }
    }
    for m in &o.plan {
        at(&mut out, &m.from).refile = Some((Change::of(m), m.to.clone()));
    }
    out
}

struct Row {
    album: LibraryAlbum,
    marks: Option<Marks>,
}

impl Row {
    fn spare(&self) -> Option<&str> {
        self.marks.as_ref()?.dup.as_ref()?.spare.as_deref()
    }
    fn kept(&self) -> bool {
        self.marks
            .as_ref()
            .and_then(|m| m.dup.as_ref())
            .is_some_and(|d| d.spare.is_none())
    }
    fn refile(&self) -> Option<&(Change, Option<String>)> {
        self.marks.as_ref()?.refile.as_ref()
    }
}

/// The albums the filter admits, newest import first, with their marks.
async fn listing(s: &UiState, filter: &Filter) -> anyhow::Result<Vec<Row>> {
    let library = &s.app.library;
    let albums = library.albums(&filter.terms()).await?;
    let overview = library.overview().await?;
    let marks = marks(&overview);
    Ok(albums
        .into_iter()
        .map(|album| Row {
            marks: marks.get(&album.path).cloned(),
            album,
        })
        .filter(|r| match filter.show.as_str() {
            "dups" => r.marks.as_ref().is_some_and(|m| m.dup.is_some()),
            "moves" => r.refile().is_some(),
            _ => true,
        })
        .collect())
}

#[derive(Template)]
#[template(path = "library.html")]
struct LibraryView {
    root: String,
    filter: Filter,
    rows: Vec<Row>,
    total: usize,
    /// Spare copies among everything the filter admits.
    spares: usize,
    newer: Option<String>,
    older: Option<String>,
    error: Option<String>,
}

impl LibraryView {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn imported(&self, a: &LibraryAlbum) -> String {
        imported(a.added)
    }
    fn first(&self) -> usize {
        self.filter.page * PAGE + 1
    }
    fn last(&self) -> usize {
        self.filter.page * PAGE + self.rows.len()
    }
}

async fn list_html(s: &UiState, filter: &Filter) -> String {
    let root = s.app.library.root().to_string_lossy().into_owned();
    let view = match listing(s, filter).await {
        Ok(rows) => {
            let total = rows.len();
            let spares = rows.iter().filter(|r| r.spare().is_some()).count();
            let page = filter.page.min(total.saturating_sub(1) / PAGE);
            let filter = filter.at_page(page);
            LibraryView {
                newer: (page > 0).then(|| filter.at_page(page - 1).url()),
                older: (total > (page + 1) * PAGE).then(|| filter.at_page(page + 1).url()),
                rows: rows.into_iter().skip(page * PAGE).take(PAGE).collect(),
                filter,
                total,
                spares,
                root,
                error: None,
            }
        }
        Err(e) => LibraryView {
            root,
            filter: filter.clone(),
            rows: Vec::new(),
            total: 0,
            spares: 0,
            newer: None,
            older: None,
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

/// The list the address describes.
async fn view(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let filter = Filter::from_fields(&q);
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="library"><p class="note">Reading the library…</p></div>"#));
        yield Ok(patch(&list_html(&s, &filter).await));
        yield Ok(super::place("library", &filter.url()));
    })
    .into_response()
}

async fn find(State(s): State<UiState>, body: Bytes) -> Response {
    let filter = Filter::from_fields(&fields(&body));
    super::many(vec![
        patch(&list_html(&s, &filter).await),
        patch(r#"<div id="lib-preview"></div>"#),
        super::place("library", &filter.url()),
    ])
}

/// What can be done to the albums ticked in the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    Refile,
    Credit,
    Enrich,
    Reimport,
    Bin,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refile" => Self::Refile,
            "credit" => Self::Credit,
            "enrich" => Self::Enrich,
            "reimport" => Self::Reimport,
            "bin" => Self::Bin,
            _ => return None,
        })
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Refile => "refile",
            Self::Credit => "credit",
            Self::Enrich => "enrich",
            Self::Reimport => "reimport",
            Self::Bin => "bin",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Refile => "Re-file",
            Self::Credit => "Credit artists",
            Self::Enrich => "Enrich",
            Self::Reimport => "Re-import",
            Self::Bin => "Bin spares",
        }
    }

    fn doing(self) -> &'static str {
        match self {
            Self::Refile => "Re-filing",
            Self::Credit => "Crediting artists on",
            Self::Enrich => "Enriching",
            Self::Reimport => "Re-importing",
            Self::Bin => "Binning",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Self::Refile => "Re-filed",
            Self::Credit => "Credited artists on",
            Self::Enrich => "Enriched",
            Self::Reimport => "Re-imported",
            Self::Bin => "Binned",
        }
    }

    /// What the preview says before the albums.
    fn about(self) -> &'static str {
        match self {
            Self::Refile => {
                "Each file moves to where the current naming rules put it. Albums the rules cannot place stay where they are."
            }
            Self::Credit => {
                "Each artist on a collaboration gets a credit of their own (ARTISTS, ALBUMARTISTS and their MusicBrainz ids). The artist and album artist tags read as before."
            }
            Self::Enrich => {
                "Genres, lyrics and artist credits as listed. ReplayGain is measured from the audio as it is applied, so it is not shown here."
            }
            Self::Reimport => {
                "Each album is matched against MusicBrainz again, retagged from the release shown, and re-filed if its new tags put it elsewhere. Albums the tagger would not apply on its own are left unticked."
            }
            Self::Bin => {
                "Spare copies move to the bin beside the library, out of sight of the shares and the player. Nothing is deleted. Kept copies in the selection stay."
            }
        }
    }

    /// How many albums one preview works out. Re-file and bin are worked
    /// out locally; the others ask MusicBrainz and LRCLIB, seconds an album.
    fn cap(self) -> usize {
        match self {
            Self::Refile | Self::Bin => 500,
            Self::Credit | Self::Enrich | Self::Reimport => 50,
        }
    }
}

/// The albums ticked in the list, or with `all`, every album the filter
/// admits, in list order.
async fn selection(s: &UiState, form: &[(String, String)]) -> anyhow::Result<(Filter, Vec<Row>)> {
    let filter = Filter::from_fields(form);
    let mut rows = listing(s, &filter).await?;
    if super::field(form, "all").is_none() {
        let ticked: std::collections::HashSet<&str> = form
            .iter()
            .filter(|(k, _)| k == "path")
            .map(|(_, v)| v.as_str())
            .collect();
        rows.retain(|r| ticked.contains(r.album.path.as_str()));
    }
    Ok((filter, rows))
}

/// One album in a preview: what applying would do to it.
struct Planned {
    key: String,
    path: String,
    title: String,
    /// Whether applying would do anything to it at all.
    applicable: bool,
    /// Ticked to begin with: applicable, and nothing that wants a look.
    ticked: bool,
    /// Why it would be left alone, or what to check.
    note: Option<String>,
    /// Sent with the album when applying: the release a re-import applies,
    /// the copy a spare is a spare of.
    with: Option<String>,
    body: Body,
}

enum Body {
    Nothing,
    Refile {
        change: &'static str,
        into: Option<String>,
        moves: Vec<(String, String)>,
    },
    Credit(Vec<CreditWrite>),
    Enrich {
        credits: Vec<CreditWrite>,
        genres: Vec<String>,
        lyrics: Vec<Lyric>,
    },
    Reimport(Box<sift::Comparison>),
    Bin {
        to: String,
        keep: String,
        reason: String,
    },
}

struct Lyric {
    file: String,
    synced: bool,
    lines: usize,
    first: String,
}

impl Lyric {
    fn of(l: &sift::PlannedLyrics) -> Self {
        let text: Vec<&str> = l
            .text
            .lines()
            .map(|line| {
                // LRC timestamps: "[01:02.03] words".
                let mut s = line.trim();
                while s.starts_with('[')
                    && let Some(end) = s.find(']')
                {
                    s = s[end + 1..].trim_start();
                }
                s
            })
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            file: l
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            synced: l.synced,
            lines: text.len(),
            first: text.first().map(|s| s.to_string()).unwrap_or_default(),
        }
    }
}

#[derive(Template)]
#[template(path = "library_plan.html")]
struct PlanView<'a> {
    action: Action,
    filter: &'a Filter,
    selected: usize,
    shown: usize,
}

#[derive(Template)]
#[template(path = "library_planned.html")]
struct PlannedView<'a> {
    p: &'a Planned,
    root: &'a str,
}

impl PlannedView<'_> {
    fn rel<'b>(&self, p: &'b str) -> &'b str {
        rel(self.root, p)
    }
    fn secs(&self, d: &Option<f64>) -> String {
        match *d {
            Some(d) if d.abs() >= 1.0 => format!("{d:+.0}s"),
            Some(_) => String::new(),
            None => "length unknown".into(),
        }
    }
    fn close(&self, d: &f64) -> bool {
        *d < 0.15
    }
}

fn title_of(a: &LibraryAlbum) -> String {
    match (&a.album_artist, &a.album) {
        (Some(artist), Some(album)) => format!("{artist} — {album}"),
        (None, Some(album)) => album.clone(),
        _ => a.path.rsplit('/').next().unwrap_or_default().to_string(),
    }
}

/// Work out what `action` would do to one album, changing nothing.
async fn plan_one(s: &UiState, action: Action, row: &Row, known: &sift::credits::Known) -> Planned {
    let library = &s.app.library;
    let root = library.root().to_string_lossy().into_owned();
    let path = row.album.path.clone();
    let mut p = Planned {
        key: key(&path),
        title: title_of(&row.album),
        path: path.clone(),
        applicable: false,
        ticked: false,
        note: None,
        with: None,
        body: Body::Nothing,
    };
    let result: anyhow::Result<()> = async {
        match action {
            Action::Refile => match library.preview(&by_path([path.as_str()])).await? {
                Some(Preview::Moves(moves)) => {
                    let dirs: std::collections::HashSet<_> =
                        moves.iter().map(|(_, to)| to.parent()).collect();
                    let one_dir = dirs.len() == 1;
                    let name = |f: &std::path::Path| {
                        if one_dir {
                            f.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            rel(&root, &f.to_string_lossy()).to_string()
                        }
                    };
                    p.body = Body::Refile {
                        change: row.refile().map_or("", |(c, _)| c.label()),
                        into: moves
                            .first()
                            .and_then(|(_, to)| to.parent())
                            .filter(|_| one_dir)
                            .map(|d| rel(&root, &d.to_string_lossy()).to_string()),
                        moves: moves.iter().map(|(f, t)| (name(f), name(t))).collect(),
                    };
                    p.applicable = true;
                    p.ticked = true;
                }
                Some(Preview::Refused(why)) => {
                    p.note = Some(format!(
                        "Left where it is: {}",
                        why.replace(&format!("{root}/"), "")
                    ))
                }
                Some(Preview::InPlace) => p.note = Some("Already where the rules file it.".into()),
                None => p.note = Some("No longer in the library.".into()),
            },
            Action::Credit => {
                let plan = library
                    .credit_plan(&by_path([path.as_str()]), known)
                    .await?;
                match plan.into_iter().next() {
                    Some(c) if !c.planned.is_empty() => {
                        p.applicable = true;
                        p.ticked = true;
                        p.note = (!c.problems.is_empty()).then(|| c.problems.join("; "));
                        p.body = Body::Credit(c.planned);
                    }
                    Some(c) => p.note = Some(c.problems.join("; ")),
                    None => {
                        p.note = Some(
                            "Nothing to credit: already credited, one artist, or a credit neither a release nor the library's artists split."
                                .into(),
                        )
                    }
                }
            }
            Action::Enrich => {
                let plan = library.enrich_plan(&path, known).await?;
                p.applicable = true;
                p.ticked = true;
                p.note = (!plan.problems.is_empty()).then(|| plan.problems.join("; "));
                p.body = Body::Enrich {
                    credits: plan.credits.files.iter().map(CreditWrite::from).collect(),
                    genres: plan.genres.clone(),
                    lyrics: plan.lyrics.iter().map(Lyric::of).collect(),
                };
            }
            Action::Reimport => {
                let r = library.rematch(&path).await?;
                match r.best {
                    Some(best) => {
                        p.applicable = true;
                        p.ticked = r.review.is_none();
                        p.note = r
                            .review
                            .map(|why| format!("The tagger would not apply this on its own: {why}."));
                        p.with = Some(best.release.id.clone());
                        p.body = Body::Reimport(Box::new(best));
                    }
                    None => p.note = Some("No release found on MusicBrainz.".into()),
                }
            }
            Action::Bin => match row.marks.as_ref().and_then(|m| m.dup.as_ref()) {
                Some(Dup {
                    keep,
                    spare: Some(reason),
                }) => match library.bin_destination(&path) {
                    Ok(to) => {
                        p.applicable = true;
                        p.ticked = true;
                        p.with = Some(keep.clone());
                        p.body = Body::Bin {
                            to: to.to_string_lossy().into_owned(),
                            keep: keep.clone(),
                            reason: reason.clone(),
                        };
                    }
                    Err(why) => p.note = Some(format!("Cannot be binned: {why}.")),
                },
                Some(_) => p.note = Some("The copy kept; it stays.".into()),
                None => p.note = Some("Not held twice; nothing to bin.".into()),
            },
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        p.applicable = false;
        p.ticked = false;
        p.note = Some(format!("{e:#}"));
    }
    p
}

/// Work out what an action would do to each selected album, streaming each
/// into the preview as it is known. Nothing is changed; the preview ends
/// with the button that applies it.
async fn preview(State(s): State<UiState>, Path(action): Path<String>, body: Bytes) -> Response {
    let Some(action) = Action::parse(&action) else {
        return failed(&anyhow::anyhow!("no such action"));
    };
    let form = fields(&body);
    let (filter, rows) = match selection(&s, &form).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    if rows.is_empty() {
        return failed(&anyhow::anyhow!("nothing selected"));
    }
    let root = s.app.library.root().to_string_lossy().into_owned();
    sse(async_stream::stream! {
        let selected = rows.len();
        let shown = selected.min(action.cap());
        let shell = PlanView { action, filter: &filter, selected, shown };
        yield Ok(patch(&shell.render().unwrap_or_default()));
        let known = if matches!(action, Action::Credit | Action::Enrich) {
            match s.app.library.known_artists().await {
                Ok(k) => k,
                Err(e) => {
                    yield Ok(patch(&super::flash_err(&format!("{e:#}"))));
                    return;
                }
            }
        } else {
            Default::default()
        };
        let mut applicable = 0;
        for (i, row) in rows.iter().take(shown).enumerate() {
            yield Ok(patch(&format!(
                r#"<p id="pv-progress" class="note">Working out {} of {shown}: {}</p>"#,
                i + 1,
                escape(&title_of(&row.album)),
            )));
            let p = plan_one(&s, action, row, &known).await;
            applicable += usize::from(p.applicable);
            let html = PlannedView { p: &p, root: &root }.render().unwrap_or_default();
            yield Ok(super::append("#pv-albums", &html));
        }
        yield Ok(patch(&format!(
            r#"<p id="pv-progress" class="note">{applicable} of {shown} would change.</p>"#
        )));
        let button = if applicable == 0 {
            String::new()
        } else {
            format!(
                r#"<button type="button" class="primary" data-indicator="_applying" data-attr:disabled="$_apply == 0 || $_applying" data-class:busy="$_applying" data-text="'{name} ' + $_apply + ($_apply == 1 ? ' album' : ' albums')" data-on:click="@post('/library/apply/{slug}', {{contentType: 'form', selector: '#lib-apply'}})">{name}</button>"#,
                name = action.name(),
                slug = action.slug(),
            )
        };
        yield Ok(patch(&format!(
            r#"<div id="pv-apply" class="actions" data-init="$_apply = document.querySelectorAll('#lib-apply input[name=path]:checked').length">{button}<button type="button" class="quiet" data-on:click="document.getElementById('lib-preview').replaceChildren()">Cancel</button></div>"#
        )));
    })
    .into_response()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A background run of one action over albums, and how far it has got.
/// Each album takes from a moment (a move) to seconds (a MusicBrainz
/// lookup, decoding for gain), so it runs apart from the request that
/// started it, and the page follows it.
#[derive(Default)]
pub(super) struct LibraryRun {
    action: Option<Action>,
    total: usize,
    done: usize,
    current: Option<String>,
    running: bool,
    results: Vec<Outcome>,
    error: Option<String>,
}

struct Outcome {
    path: String,
    ok: bool,
    detail: String,
    problems: Vec<String>,
}

#[derive(Template)]
#[template(path = "library_run.html")]
struct RunView<'a> {
    run: &'a LibraryRun,
    root: String,
}

impl RunView<'_> {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn recent(&self) -> impl Iterator<Item = &Outcome> {
        self.run.results.iter().rev().take(30)
    }
    fn verb(&self) -> &'static str {
        match self.run.action {
            Some(a) if self.run.running => a.doing(),
            Some(a) => a.done(),
            None => "",
        }
    }
    fn failed(&self) -> usize {
        self.run.results.iter().filter(|r| !r.ok).count()
    }
}

pub(super) fn run_html(s: &UiState) -> String {
    let run = s.run.lock();
    RunView {
        run: &run,
        root: s.app.library.root().to_string_lossy().into_owned(),
    }
    .render()
    .unwrap_or_default()
}

/// Apply an action to the albums ticked in its preview, in the background.
/// The response follows the run and, when it ends, draws the list again.
async fn apply(State(s): State<UiState>, Path(action): Path<String>, body: Bytes) -> Response {
    let Some(action) = Action::parse(&action) else {
        return failed(&anyhow::anyhow!("no such action"));
    };
    let form = fields(&body);
    let filter = Filter::from_fields(&form);
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, v)| k == "path" && !v.is_empty())
        .map(|(_, v)| v.clone())
        .collect();
    if paths.is_empty() {
        return failed(&anyhow::anyhow!("nothing ticked"));
    }
    // What the preview attached to each album, by its key.
    let with: HashMap<String, String> = form
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("with-")?.to_string(), v.clone())))
        .collect();
    {
        let mut run = s.run.lock();
        if run.running {
            return failed(&anyhow::anyhow!(
                "Already {} albums; start another when it finishes.",
                run.action
                    .map_or("working on", |a| a.doing())
                    .to_lowercase()
            ));
        }
        *run = LibraryRun {
            action: Some(action),
            total: paths.len(),
            running: true,
            ..Default::default()
        };
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (app, state) = (s.app.clone(), s.run.clone());
    tokio::spawn(async move {
        if let Err(e) = run(&app, &state, action, &paths, &with).await {
            state.lock().error = Some(format!("{e:#}"));
        }
        {
            let mut run = state.lock();
            run.current = None;
            run.running = false;
        }
        let _ = tx.send(());
    });
    sse(async_stream::stream! {
        yield Ok(patch(&run_html(&s)));
        yield Ok(patch(r#"<div id="lib-preview"></div>"#));
        let _ = rx.await;
        yield Ok(patch(&run_html(&s)));
        yield Ok(patch(&list_html(&s, &filter).await));
        let (done, failed) = {
            let run = s.run.lock();
            (run.done, run.results.iter().filter(|r| !r.ok).count())
        };
        let msg = format!(
            "{} {} albums{}",
            action.done(),
            done - failed,
            if failed > 0 { format!(", {failed} failed") } else { String::new() }
        );
        yield Ok(patch(&flash_ok(&msg)));
    })
    .into_response()
}

async fn run(
    app: &crate::App,
    state: &parking_lot::Mutex<LibraryRun>,
    action: Action,
    paths: &[String],
    with: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let library = &app.library;
    let root = library.root().to_string_lossy().into_owned();
    let record = |o: Outcome| {
        let mut run = state.lock();
        run.results.push(o);
        run.done += 1;
    };
    if action == Action::Refile {
        // One pass under the index's writer, each album planned again as it
        // moves.
        return library
            .refile_each(&by_path(paths.iter().map(String::as_str)), |m| {
                record(Outcome {
                    ok: m.to.is_some() || m.refused.is_none(),
                    detail: match (&m.to, &m.refused) {
                        (Some(to), _) => format!("→ {}", rel(&root, to)),
                        (None, Some(why)) => why.replace(&format!("{root}/"), ""),
                        (None, None) => "Already where the rules file it.".into(),
                    },
                    path: m.from,
                    problems: Vec::new(),
                });
            })
            .await;
    }
    let known = match action {
        Action::Credit | Action::Enrich => library.known_artists().await?,
        _ => Default::default(),
    };
    for path in paths {
        state.lock().current = Some(path.clone());
        let attached = with.get(&key(path)).map(String::as_str);
        let query = by_path([path.as_str()]);
        let outcome: anyhow::Result<(String, Vec<String>)> = async {
            Ok(match action {
                Action::Refile => unreachable!("re-filed above"),
                Action::Credit => match library.credit_with(&query, &known).await?.pop() {
                    Some(c) => (
                        format!("{} files: {}", c.files, c.artists.join(", ")),
                        c.problems,
                    ),
                    None => ("nothing to credit".into(), Vec::new()),
                },
                Action::Enrich => match library.enrich_with(&query, &known).await?.pop() {
                    Some(e) => {
                        let mut parts = vec![
                            e.gain_db
                                .map_or_else(|| "no gain".to_string(), |g| format!("{g:+.1} dB")),
                        ];
                        if !e.genres.is_empty() {
                            parts.push(e.genres.join(", "));
                        }
                        parts.push(format!("lyrics for {}", e.lyrics));
                        if !e.artists.is_empty() {
                            parts.push(format!("credited {}", e.artists.join(", ")));
                        }
                        (parts.join(" · "), e.problems)
                    }
                    None => anyhow::bail!("no longer in the library"),
                },
                Action::Reimport => {
                    let Some(release) = attached else {
                        anyhow::bail!("no release chosen");
                    };
                    let r = library.reimport(path, Some(release)).await?;
                    let release = r
                        .release
                        .map(|m| format!("{} — {}", m.artist, m.title))
                        .unwrap_or_default();
                    match (r.to, r.review) {
                        (Some(to), _) => (format!("{release} → {}", rel(&root, &to)), r.problems),
                        (None, Some(why)) => anyhow::bail!("left for review: {why}"),
                        (None, None) => (release, r.problems),
                    }
                }
                Action::Bin => {
                    let Some(keep) = attached else {
                        anyhow::bail!("not a spare copy");
                    };
                    let sets = library
                        .duplicates(&by_path([keep, path.as_str()]), true)
                        .await?;
                    match sets
                        .iter()
                        .flat_map(|d| &d.spares)
                        .find(|sp| sp.album.path == *path)
                        .and_then(|sp| sp.binned_to.clone())
                    {
                        Some(to) => (format!("→ {to}"), Vec::new()),
                        None => anyhow::bail!("no longer a spare copy; left where it is"),
                    }
                }
            })
        }
        .await;
        record(match outcome {
            Ok((detail, problems)) => Outcome {
                path: path.clone(),
                ok: true,
                detail,
                problems,
            },
            Err(e) => Outcome {
                path: path.clone(),
                ok: false,
                detail: format!("{e:#}"),
                problems: Vec::new(),
            },
        });
    }
    Ok(())
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

    fn filter(since: &str, from: &str) -> Filter {
        Filter {
            since: since.into(),
            from: from.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_import_date_becomes_a_lower_bound() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-18T15:30:00Z")
            .unwrap()
            .to_utc();
        let midnight = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        assert_eq!(
            filter("today", "").since_secs(now),
            Some(midnight.timestamp())
        );
        assert_eq!(
            filter("week", "").since_secs(now),
            Some(now.timestamp() - 7 * 86_400)
        );
        assert_eq!(
            filter("date", "2026-09-01").since_secs(now),
            Some(1_788_220_800)
        );
        assert_eq!(filter("date", "not a date").since_secs(now), None);
        assert_eq!(filter("", "").since_secs(now), None);
    }

    #[test]
    fn the_list_is_newest_first_after_any_sort_asked_for() {
        let f = Filter {
            q: "artist:burial year-".into(),
            ..Default::default()
        };
        assert_eq!(
            f.terms(),
            ["artist:burial", "year-", "added-", "albumartist+", "album+"]
        );
        let q = sift::library::Query::parse(&f.terms()).unwrap();
        assert_eq!(q.sort[0], ("year".to_string(), false));
        assert_eq!(q.sort[1], ("added".to_string(), false));
    }

    #[test]
    fn the_address_round_trips_the_filter() {
        let f = Filter {
            q: "burial".into(),
            since: "date".into(),
            from: "2026-09-01".into(),
            show: "dups".into(),
            page: 2,
        };
        let url = f.url();
        let back: Vec<(String, String)> =
            url::form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(Filter::from_fields(&back), f);
    }

    #[test]
    fn lyrics_are_summarised_without_their_timestamps() {
        let l = Lyric::of(&sift::PlannedLyrics {
            path: "/m/a/01 x.flac".into(),
            synced: true,
            text: "[00:01.00] First line\n[00:02.00]\n[00:03.00][00:09.00] Second".into(),
        });
        assert_eq!((l.lines, l.first.as_str()), (2, "First line"));
        assert_eq!(l.file, "01 x.flac");
    }
}
