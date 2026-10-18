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
use crate::library::{AlbumMove, CreditWrite, LibraryAlbum, Overview, Preview, TagEdits};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/library", get(view))
        .route("/library/find", post(find))
        .route("/library/album", get(album))
        .route("/library/album/releases", get(releases))
        .route("/library/album/edit", get(edit_form))
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
    /// The album open in detail, by path; it does not narrow the list.
    album: String,
}

impl Filter {
    fn from_fields(f: &[(String, String)]) -> Self {
        let get = |k| super::field(f, k).unwrap_or_default().trim().to_string();
        Self {
            q: get("q"),
            since: get("since"),
            from: get("from"),
            show: get("show"),
            album: get("album"),
        }
    }

    /// The earliest import time the filter admits, in seconds. Days,
    /// weeks (from Monday) and months start at midnight where `now` is.
    fn since_secs<Tz: chrono::TimeZone>(&self, now: chrono::DateTime<Tz>) -> Option<i64> {
        let tz = now.timezone();
        let day = |d: chrono::NaiveDate| {
            tz.from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
                .earliest()
                .map(|t| t.timestamp())
        };
        match self.since.as_str() {
            "today" => day(now.date_naive()),
            "week" => {
                use chrono::Datelike;
                let today = now.date_naive();
                day(today - chrono::Duration::days(today.weekday().num_days_from_monday().into()))
            }
            "month" => {
                use chrono::Datelike;
                day(now.date_naive().with_day(1)?)
            }
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
        if let Some(t) = self.since_secs(chrono::Local::now()) {
            terms.push(format!("added:{t}.."));
        }
        terms.extend(["added-", "albumartist+", "album+"].map(String::from));
        terms
    }

    fn url(&self) -> String {
        super::place_url("/library", &self.params())
    }

    /// The filter as address parameters, the open album included.
    fn params(&self) -> Vec<(&str, &str)> {
        let from = if self.since == "date" {
            self.from.as_str()
        } else {
            ""
        };
        vec![
            ("q", &self.q),
            ("since", &self.since),
            ("from", from),
            ("show", &self.show),
            ("album", &self.album),
        ]
    }

    /// The same list with `path` open in detail, and the address that
    /// draws just that album.
    fn opening(&self, path: &str) -> (String, String) {
        let open = Self {
            album: path.to_string(),
            ..self.clone()
        };
        (
            open.url(),
            super::place_url("/library/album", &open.params()),
        )
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
    Ok(rows(s, &filter.terms())
        .await?
        .into_iter()
        .filter(|r| match filter.show.as_str() {
            "dups" => r.marks.as_ref().is_some_and(|m| m.dup.is_some()),
            "moves" => r.refile().is_some(),
            _ => true,
        })
        .collect())
}

/// The albums `terms` matches, with their marks.
async fn rows(s: &UiState, terms: &[String]) -> anyhow::Result<Vec<Row>> {
    let library = &s.app.library;
    let albums = library.albums(terms).await?;
    let overview = library.overview().await?;
    let marks = marks(&overview);
    Ok(albums
        .into_iter()
        .map(|album| Row {
            marks: marks.get(&album.path).cloned(),
            album,
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
    error: Option<String>,
}

impl LibraryView {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn imported(&self, a: &LibraryAlbum) -> String {
        imported(a.added)
    }
    fn opening(&self, path: &str) -> (String, String) {
        self.filter.opening(path)
    }
}

async fn list_html(s: &UiState, filter: &Filter) -> String {
    let root = s.app.library.root().to_string_lossy().into_owned();
    let listed = if filter.since == "date" && filter.since_secs(chrono::Local::now()).is_none() {
        Err(anyhow::anyhow!("Pick the date to list imports since."))
    } else {
        listing(s, filter).await
    };
    let view = match listed {
        Ok(rows) => {
            let total = rows.len();
            let spares = rows.iter().filter(|r| r.spare().is_some()).count();
            LibraryView {
                rows,
                filter: filter.clone(),
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
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

/// The selection's counts, cleared with the list. A redrawn list keeps its
/// form's `data-signals` attribute unchanged, so it does not reset them.
fn unselected() -> axum::response::sse::Event {
    super::signals(r#"{"_sel": 0, "_spares": 0}"#)
}

/// The list the address describes.
async fn view(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let filter = Filter::from_fields(&q);
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="library"><p class="note">Reading the library…</p></div>"#));
        yield Ok(patch(&list_html(&s, &filter).await));
        yield Ok(unselected());
        if !filter.album.is_empty() {
            yield Ok(patch(&album_html(&s, &filter).await));
        }
        yield Ok(super::place("library", &filter.url()));
    })
    .into_response()
}

async fn find(State(s): State<UiState>, body: Bytes) -> Response {
    let filter = Filter::from_fields(&fields(&body));
    super::many(vec![
        patch(&list_html(&s, &filter).await),
        unselected(),
        patch(r#"<div id="lib-preview"></div>"#),
        patch(r#"<div id="lib-album"></div>"#),
        super::place(
            "library",
            &Filter {
                album: String::new(),
                ..filter
            }
            .url(),
        ),
    ])
}

#[derive(Template)]
#[template(path = "library_album.html")]
struct AlbumView<'a> {
    root: &'a str,
    filter: &'a Filter,
    row: &'a Row,
    tracks: Vec<sift::meta::Track>,
}

impl AlbumView<'_> {
    fn rel<'b>(&self, p: &'b str) -> &'b str {
        rel(self.root, p)
    }
    fn title(&self) -> String {
        title_of(&self.row.album)
    }
    /// The address with no album open.
    fn closed(&self) -> String {
        Filter {
            album: String::new(),
            ..self.filter.clone()
        }
        .url()
    }
    fn file(&self, t: &sift::meta::Track) -> String {
        t.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
    fn length(&self, t: &sift::meta::Track) -> String {
        let s = t.duration.as_secs();
        format!("{}:{:02}", s / 60, s % 60)
    }
    /// Every tag the file carries that sift reads, as a person names it.
    fn tags(&self, t: &sift::meta::Track) -> Vec<(&'static str, String)> {
        let list = |v: &[String]| v.join("; ");
        let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        let o = |v: &Option<String>| v.clone().unwrap_or_default();
        [
            ("title", o(&t.title)),
            ("artist", o(&t.artist)),
            ("artists", list(&t.artists)),
            ("album", o(&t.album)),
            ("album artist", o(&t.album_artist)),
            ("album artists", list(&t.album_artists)),
            ("track", num(t.track)),
            ("tracks", num(t.track_total)),
            ("disc", num(t.disc)),
            ("discs", num(t.disc_total)),
            ("date", o(&t.date)),
            ("original date", o(&t.original_date)),
            ("genre", o(&t.genre)),
            ("label", o(&t.label)),
            ("catalogue number", o(&t.catalog_number)),
            ("country", o(&t.country)),
            ("media", o(&t.media)),
            (
                "compilation",
                if t.compilation {
                    "yes".into()
                } else {
                    String::new()
                },
            ),
            ("MusicBrainz release", o(&t.mb_album_id)),
            ("MusicBrainz release group", o(&t.mb_release_group_id)),
            ("MusicBrainz recording", o(&t.mb_recording_id)),
            ("MusicBrainz track", o(&t.mb_track_id)),
            ("MusicBrainz artists", list(&t.mb_artist_ids)),
            ("MusicBrainz album artists", list(&t.mb_album_artist_ids)),
            (
                "audio",
                [
                    t.format.clone(),
                    t.bit_depth.map(|b| format!("{b}-bit")).unwrap_or_default(),
                    t.sample_rate
                        .map(|r| format!("{:.1} kHz", r as f64 / 1000.0))
                        .unwrap_or_default(),
                    t.bitrate.map(|b| format!("{b} kbps")).unwrap_or_default(),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            ),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect()
    }
}

/// The album `filter.album` names, open: its files and their tags, where
/// the rules file it, and what can be done to it.
async fn album_html(s: &UiState, filter: &Filter) -> String {
    let root = s.app.library.root().to_string_lossy().into_owned();
    let result = async {
        let row = rows(s, &by_path([filter.album.as_str()]))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No longer in the library."))?;
        let tracks = s.app.library.tracks(&row.album.path).await?;
        anyhow::Ok((row, tracks))
    }
    .await;
    match result {
        Ok((row, tracks)) => AlbumView {
            root: &root,
            filter,
            row: &row,
            tracks,
        }
        .render()
        .unwrap_or_default(),
        Err(e) => format!(
            r#"<div id="lib-album" class="job"><p class="error">{}</p></div>"#,
            escape(&format!("{e:#}"))
        ),
    }
}

/// One album open in detail, above the list.
async fn album(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let filter = Filter::from_fields(&q);
    super::many(vec![
        patch(&album_html(&s, &filter).await),
        patch(r#"<div id="lib-preview"></div>"#),
        super::place("library", &filter.url()),
    ])
}

#[derive(Template)]
#[template(path = "library_releases.html")]
struct ReleasesView<'a> {
    path: &'a str,
    filter: &'a Filter,
    candidates: Vec<sift::Candidate>,
    review: Option<String>,
    error: Option<String>,
}

/// The releases MusicBrainz has for an album's files, closest first, to
/// re-tag it from; or one pasted.
async fn releases(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let filter = Filter::from_fields(&q);
    let path = super::field(&q, "path").unwrap_or_default().to_string();
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="lib-retag"><p class="note">Asking MusicBrainz for releases that fit these files…</p></div>"#));
        let view = match s.app.library.rematch(&path).await {
            Ok(r) => ReleasesView { path: &path, filter: &filter, candidates: r.candidates, review: r.review, error: None },
            Err(e) => ReleasesView { path: &path, filter: &filter, candidates: Vec::new(), review: None, error: Some(format!("{e:#}")) },
        };
        yield Ok(patch(&view.render().unwrap_or_default()));
    })
    .into_response()
}

#[derive(Template)]
#[template(path = "library_edit.html")]
struct EditView<'a> {
    path: &'a str,
    filter: &'a Filter,
    tracks: Vec<sift::meta::Track>,
}

impl EditView<'_> {
    fn fields(&self) -> &'static [(&'static str, &'static str)] {
        ALBUM_FIELDS
    }
    fn now(&self, field: &str) -> String {
        consensus(&self.tracks, field).0.unwrap_or_default()
    }
    fn varies(&self, field: &str) -> bool {
        consensus(&self.tracks, field).1
    }
    fn value(&self, t: &sift::meta::Track, field: &str) -> String {
        t.field(field).unwrap_or_default()
    }
    fn file(&self, t: &sift::meta::Track) -> String {
        t.path.to_string_lossy().into_owned()
    }
}

/// The album's tags as a form: album-wide fields, then each file's own.
async fn edit_form(State(s): State<UiState>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let filter = Filter::from_fields(&q);
    let path = super::field(&q, "path").unwrap_or_default().to_string();
    match s.app.library.tracks(&path).await {
        Ok(tracks) => super::one(
            EditView {
                path: &path,
                filter: &filter,
                tracks,
            }
            .render()
            .unwrap_or_default(),
        ),
        Err(e) => failed(&e),
    }
}

/// What can be done to the albums ticked in the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    Refile,
    Credit,
    Enrich,
    Reimport,
    Bin,
    /// Tags typed by hand, on one album.
    Edit,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refile" => Self::Refile,
            "credit" => Self::Credit,
            "enrich" => Self::Enrich,
            "reimport" => Self::Reimport,
            "bin" => Self::Bin,
            "edit" => Self::Edit,
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
            Self::Edit => "edit",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Refile => "Re-file",
            Self::Credit => "Credit artists",
            Self::Enrich => "Enrich",
            Self::Reimport => "Re-tag",
            Self::Bin => "Bin spares",
            Self::Edit => "Edit tags",
        }
    }

    /// The apply button's words, before the count.
    fn verb(self) -> &'static str {
        match self {
            Self::Credit => "Credit artists on",
            Self::Bin => "Bin",
            Self::Edit => "Edit",
            _ => self.name(),
        }
    }

    fn doing(self) -> &'static str {
        match self {
            Self::Refile => "Re-filing",
            Self::Credit => "Crediting artists on",
            Self::Enrich => "Enriching",
            Self::Reimport => "Re-tagging",
            Self::Bin => "Binning",
            Self::Edit => "Editing",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Self::Refile => "Re-filed",
            Self::Credit => "Credited artists on",
            Self::Enrich => "Enriched",
            Self::Reimport => "Re-tagged",
            Self::Bin => "Binned",
            Self::Edit => "Edited",
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
                "Each album is retagged in place from the MusicBrainz release shown, every field listed as it is and as it would be, and re-filed if its new tags put it elsewhere. Nothing is downloaded. Albums the tagger would not match on its own are left unticked."
            }
            Self::Edit => {
                "The fields typed, as each file has them and as they would be, and where the new tags file the album."
            }
            Self::Bin => {
                "Spare copies move to the bin beside the library, out of sight of the shares and the player. Nothing is deleted. Kept copies in the selection stay."
            }
        }
    }

    /// How many albums one preview works out, for the actions that ask
    /// MusicBrainz or LRCLIB, seconds an album. The rest are local and
    /// preview every album selected.
    fn cap(self) -> Option<usize> {
        match self {
            Self::Refile | Self::Bin | Self::Edit => None,
            Self::Credit | Self::Enrich | Self::Reimport => Some(50),
        }
    }
}

/// The albums ticked in the list (or the one open in detail), or with
/// `all`, every album the filter admits, in list order.
async fn selection(s: &UiState, form: &[(String, String)]) -> anyhow::Result<(Filter, Vec<Row>)> {
    let filter = Filter::from_fields(form);
    if super::field(form, "all").is_some() {
        return Ok((filter.clone(), listing(s, &filter).await?));
    }
    let ticked: Vec<&str> = form
        .iter()
        .filter(|(k, v)| k == "path" && !v.is_empty())
        .map(|(_, v)| v.as_str())
        .collect();
    if ticked.is_empty() {
        return Ok((filter, Vec::new()));
    }
    let mut terms = by_path(ticked);
    terms.extend(["added-", "albumartist+", "album+"].map(String::from));
    Ok((filter, rows(s, &terms).await?))
}

/// What the person chose for a single album beyond ticking it: the
/// release to re-tag from, or the tags they typed.
#[derive(Default)]
struct Ask {
    release: Option<String>,
    edits: Option<TagEdits>,
}

/// A MusicBrainz release id from what was pasted: the id, or a link to the
/// release. Discogs ids (`discogs:123`) pass through.
fn release_id(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('/');
    let id = s.rsplit('/').next().unwrap_or(s);
    let id = id.split(['?', '#']).next().unwrap_or(id);
    let uuid = id.len() == 36
        && id.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if uuid {
        Some(id.to_ascii_lowercase())
    } else if s.starts_with("discogs:") {
        Some(s.to_string())
    } else {
        None
    }
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
    Reimport {
        release: sift::Candidate,
        files: Vec<FileChanges>,
        missing: Vec<String>,
        untouched: Vec<String>,
    },
    Edit {
        files: Vec<FileChanges>,
        into: Option<String>,
    },
    Bin {
        to: String,
        keep: String,
        reason: String,
    },
}

/// One file in a re-tag or an edit: the fields that change, and where it
/// goes when it moves.
struct FileChanges {
    file: String,
    to: Option<String>,
    changes: Vec<sift::meta::TagChange>,
}

impl FileChanges {
    fn of(
        root: &str,
        from: &std::path::Path,
        to: &std::path::Path,
        changes: Vec<sift::meta::TagChange>,
    ) -> Self {
        let name = |p: &std::path::Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        Self {
            file: name(from),
            to: (from != to).then(|| {
                if from.parent() == to.parent() {
                    name(to)
                } else {
                    rel(root, &to.to_string_lossy()).to_string()
                }
            }),
            changes,
        }
    }
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
    /// Files whose tags change.
    fn changed(&self, files: &[FileChanges]) -> usize {
        files.iter().filter(|f| !f.changes.is_empty()).count()
    }
    fn moving(&self, files: &[FileChanges]) -> bool {
        files.iter().any(|f| f.to.is_some())
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
async fn plan_one(
    s: &UiState,
    action: Action,
    row: &Row,
    known: &sift::credits::Known,
    ask: &Ask,
) -> Planned {
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
                // The release the person chose, or the closest MusicBrainz
                // has, with why the tagger would not take it on its own.
                let (release, review) = match &ask.release {
                    Some(id) => (Some(id.clone()), None),
                    None => {
                        let r = library.rematch(&path).await?;
                        (r.best.map(|b| b.release.id), r.review)
                    }
                };
                let Some(release) = release else {
                    p.note = Some("No release found on MusicBrainz. Paste one from the album's page.".into());
                    return Ok(());
                };
                let plan = library.retag_plan(&path, &release).await?;
                let changes = plan.files.iter().filter(|f| !f.changes.is_empty() || f.path != f.to).count();
                p.applicable = plan.refused.is_none() && changes > 0;
                p.ticked = p.applicable && review.is_none();
                p.note = match (&plan.refused, review, changes) {
                    (Some(why), _, _) => Some(format!("Would be refused: {}", why.replace(&format!("{root}/"), ""))),
                    (None, _, 0) => Some("The files already carry this release's tags, where it files them.".into()),
                    (None, Some(why), _) => Some(format!("The tagger would not match this on its own: {why}.")),
                    (None, None, _) => None,
                };
                p.with = Some(release);
                let c = &plan.comparison;
                p.body = Body::Reimport {
                    release: c.release.clone(),
                    files: plan
                        .files
                        .into_iter()
                        .map(|f| FileChanges::of(&root, &f.path, &f.to, f.changes))
                        .collect(),
                    missing: c
                        .missing
                        .iter()
                        .map(|m| format!("{}-{} {}", m.disc, m.position, m.title))
                        .collect(),
                    untouched: plan
                        .untouched
                        .iter()
                        .map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned())
                        .collect(),
                };
            }
            Action::Edit => {
                let Some(edits) = &ask.edits else {
                    p.note = Some("No tags typed.".into());
                    return Ok(());
                };
                let plan = library.edit_plan(&path, edits).await?;
                let moves: HashMap<_, _> = match &plan.refile {
                    Preview::Moves(m) => m.iter().cloned().collect(),
                    _ => HashMap::new(),
                };
                let files: Vec<FileChanges> = plan
                    .files
                    .into_iter()
                    .map(|(f, changes)| {
                        let to = moves.get(&f).cloned().unwrap_or_else(|| f.clone());
                        FileChanges::of(&root, &f, &to, changes)
                    })
                    .collect();
                p.applicable = files.iter().any(|f| !f.changes.is_empty());
                p.ticked = p.applicable;
                p.note = match &plan.refile {
                    Preview::Refused(why) => Some(format!(
                        "The tags change, but the album stays where it is: {}",
                        why.replace(&format!("{root}/"), "")
                    )),
                    _ if !p.applicable => Some("Nothing typed differs from the files.".into()),
                    _ => None,
                };
                p.with = serde_json::to_string(edits).ok();
                let into = moves
                    .values()
                    .next()
                    .and_then(|to| to.parent())
                    .map(|d| rel(&root, &d.to_string_lossy()).to_string())
                    .filter(|d| Some(d.as_str()) != std::path::Path::new(&path).strip_prefix(&root).ok().and_then(|p| p.to_str()));
                p.body = Body::Edit { files, into };
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
    let ask = match ask(&s, action, &form, &rows).await {
        Ok(a) => a,
        Err(e) => return failed(&e),
    };
    sse(async_stream::stream! {
        let selected = rows.len();
        let shown = action.cap().map_or(selected, |c| selected.min(c));
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
            let p = plan_one(&s, action, row, &known, &ask).await;
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
                r#"<button type="button" class="primary" data-indicator="_applying" data-attr:disabled="$_apply == 0 || $_applying" data-class:busy="$_applying" data-text="'{verb} ' + $_apply + ($_apply == 1 ? ' album' : ' albums')" data-on:click="@post('/library/apply/{slug}', {{contentType: 'form', selector: '#lib-apply'}})">{name}</button>"#,
                name = action.name(),
                verb = action.verb(),
                slug = action.slug(),
            )
        };
        yield Ok(patch(&format!(
            r#"<div id="pv-apply" class="actions" data-init="$_apply = document.querySelectorAll('#lib-apply input[name=path]:checked').length">{button}<button type="button" class="quiet" data-on:click="document.getElementById('lib-preview').replaceChildren()">Cancel</button></div>"#
        )));
    })
    .into_response()
}

/// Album-wide fields the edit form offers, by beets name, with their labels.
const ALBUM_FIELDS: &[(&str, &str)] = &[
    ("album", "Album"),
    ("albumartist", "Album artist"),
    ("date", "Date"),
    ("original_date", "Original date"),
    ("genre", "Genre"),
    ("label", "Label"),
    ("catalognum", "Catalogue number"),
];

/// Fields the edit form offers on each file.
const TRACK_FIELDS: &[&str] = &["disc", "track", "title", "artist"];

/// What every file has for `field`, and whether they disagree.
fn consensus(tracks: &[sift::meta::Track], field: &str) -> (Option<String>, bool) {
    let mut values = tracks.iter().map(|t| t.field(field));
    let first = values.next().flatten();
    let varies = values.any(|v| v != first);
    (if varies { None } else { first }, varies)
}

/// The edits the form asks for against the files as they are now: only
/// fields typed differently from what the files have. An album-wide field
/// the files disagree on and left blank is left alone; blanking one they
/// agree on clears it.
fn edits_from(tracks: &[sift::meta::Track], form: &[(String, String)]) -> TagEdits {
    let raw = |k: &str| {
        form.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.trim().to_string())
    };
    let mut edits = TagEdits::default();
    for (field, _) in ALBUM_FIELDS {
        let Some(typed) = raw(&format!("a-{field}")) else {
            continue;
        };
        let (now, varies) = consensus(tracks, field);
        if (varies && typed.is_empty()) || (!varies && typed == now.unwrap_or_default()) {
            continue;
        }
        edits
            .album
            .push((field.to_string(), (!typed.is_empty()).then_some(typed)));
    }
    for i in 0.. {
        let Some(file) = raw(&format!("t{i}-file")) else {
            break;
        };
        let Some(t) = tracks.iter().find(|t| t.path.to_string_lossy() == file) else {
            continue;
        };
        let mut changes = Vec::new();
        for field in TRACK_FIELDS {
            let Some(typed) = raw(&format!("t{i}-{field}")) else {
                continue;
            };
            if typed != t.field(field).unwrap_or_default() {
                changes.push((field.to_string(), (!typed.is_empty()).then_some(typed)));
            }
        }
        if !changes.is_empty() {
            edits.tracks.push((t.path.clone(), changes));
        }
    }
    edits
}

/// What the form chose for a single album: a release (picked or pasted)
/// for a re-tag, or typed tags for an edit.
async fn ask(
    s: &UiState,
    action: Action,
    form: &[(String, String)],
    rows: &[Row],
) -> anyhow::Result<Ask> {
    let one = || match rows {
        [row] => Ok(row.album.path.as_str()),
        _ => Err(anyhow::anyhow!("one album at a time")),
    };
    match action {
        Action::Reimport => {
            let release = match super::field(form, "pasted") {
                Some(p) => Some(release_id(p).ok_or_else(|| {
                    anyhow::anyhow!("{p:?} is not a MusicBrainz release id or link")
                })?),
                None => super::field(form, "release").map(str::to_string),
            };
            if release.is_some() {
                one()?;
            }
            Ok(Ask {
                release,
                edits: None,
            })
        }
        Action::Edit => {
            let tracks = s.app.library.tracks(one()?).await?;
            Ok(Ask {
                release: None,
                edits: Some(edits_from(&tracks, form)),
            })
        }
        _ => Ok(Ask::default()),
    }
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
    /// Where the album is now, when the action moved it.
    to: Option<String>,
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
        self.run.results.iter().rev()
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
        use futures_util::FutureExt;
        // A panic in the run still frees the slot.
        match std::panic::AssertUnwindSafe(run(&app, &state, action, &paths, &with))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => state.lock().error = Some(format!("{e:#}")),
            Err(_) => state.lock().error = Some("stopped by an internal error".into()),
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
        // The album open in detail is drawn again where it now is.
        let mut filter = filter;
        if !filter.album.is_empty() {
            let moved = s.run.lock().results.iter().find(|r| r.path == filter.album).and_then(|r| r.to.clone());
            if let Some(to) = moved {
                filter.album = to;
            }
        }
        yield Ok(patch(&list_html(&s, &filter).await));
        yield Ok(unselected());
        if !filter.album.is_empty() {
            yield Ok(patch(&album_html(&s, &filter).await));
            yield Ok(super::place("library", &filter.url()));
        }
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
                    to: m.to.clone().filter(|to| *to != m.from),
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
        // What happened, problems on the way, and where the album went.
        let outcome: anyhow::Result<(String, Vec<String>, Option<String>)> = async {
            Ok(match action {
                Action::Refile => unreachable!("re-filed above"),
                Action::Credit => match library.credit_with(&query, &known).await?.pop() {
                    Some(c) => (
                        format!("{} files: {}", c.files, c.artists.join(", ")),
                        c.problems,
                        None,
                    ),
                    None => ("nothing to credit".into(), Vec::new(), None),
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
                        (parts.join(" · "), e.problems, None)
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
                        (Some(to), _) if to == *path => {
                            (format!("{release}, retagged in place"), r.problems, None)
                        }
                        (Some(to), _) => (
                            format!("{release} → {}", rel(&root, &to)),
                            r.problems,
                            Some(to),
                        ),
                        (None, Some(why)) => anyhow::bail!("left for review: {why}"),
                        (None, None) => (release, r.problems, None),
                    }
                }
                Action::Edit => {
                    let edits: TagEdits = attached
                        .and_then(|e| serde_json::from_str(e).ok())
                        .ok_or_else(|| anyhow::anyhow!("no edits given"))?;
                    let m = library.edit(path, &edits).await?;
                    let files = edits.album.len() + edits.tracks.len();
                    match (m.to, m.refused) {
                        (Some(to), _) if to != *path => {
                            (format!("→ {}", rel(&root, &to)), Vec::new(), Some(to))
                        }
                        (_, Some(why)) => (
                            format!("{files} edits; left where it is"),
                            vec![why.replace(&format!("{root}/"), "")],
                            None,
                        ),
                        _ => ("tags written in place".into(), Vec::new(), None),
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
                        Some(to) => (format!("→ {to}"), Vec::new(), None),
                        None => anyhow::bail!("no longer a spare copy; left where it is"),
                    }
                }
            })
        }
        .await;
        record(match outcome {
            Ok((detail, problems, to)) => Outcome {
                path: path.clone(),
                to,
                ok: true,
                detail,
                problems,
            },
            Err(e) => Outcome {
                path: path.clone(),
                to: None,
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
        // A Sunday afternoon.
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
            Some(1_791_763_200),
            "Monday 12 October"
        );
        assert_eq!(
            filter("month", "").since_secs(now),
            Some(1_790_812_800),
            "1 October"
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
            album: "/m/Burial/(2007) Untrue".into(),
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

    fn track(path: &str, title: &str, album: &str, genre: Option<&str>) -> sift::meta::Track {
        sift::meta::Track {
            path: path.into(),
            title: Some(title.into()),
            album: Some(album.into()),
            genre: genre.map(Into::into),
            ..Default::default()
        }
    }

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_edit_is_only_what_was_typed_differently() {
        let tracks = [
            track("/m/a/1.flac", "One", "Monstre", Some("Rock")),
            track("/m/a/2.flac", "Two", "Monstre", None),
        ];
        let edits = edits_from(
            &tracks,
            &form(&[
                ("a-album", "Monster"),
                // Differs between the files and left blank: untouched.
                ("a-genre", ""),
                ("a-label", ""),
                ("t0-file", "/m/a/1.flac"),
                ("t0-title", "One"),
                ("t1-file", "/m/a/2.flac"),
                ("t1-title", "Two (Live)"),
            ]),
        );
        assert_eq!(
            edits.album,
            [("album".to_string(), Some("Monster".to_string()))]
        );
        assert_eq!(
            edits.tracks,
            [(
                std::path::PathBuf::from("/m/a/2.flac"),
                vec![("title".to_string(), Some("Two (Live)".to_string()))]
            )]
        );
        let cleared = edits_from(&tracks, &form(&[("a-album", "")]));
        assert_eq!(cleared.album, [("album".to_string(), None)]);
    }

    #[test]
    fn a_release_is_its_id_or_a_link_to_it() {
        let id = "4c3e0b3c-2f8a-4a4b-9d6e-2a1f3b5c7d9e";
        assert_eq!(release_id(id).as_deref(), Some(id));
        assert_eq!(
            release_id(&format!("https://musicbrainz.org/release/{id}/")).as_deref(),
            Some(id)
        );
        assert_eq!(
            release_id(&format!(
                "https://musicbrainz.org/release/{}?tab=x",
                id.to_uppercase()
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(release_id("discogs:123").as_deref(), Some("discogs:123"));
        assert_eq!(release_id("monster"), None);
    }
}
