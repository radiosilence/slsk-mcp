//! The library as one list of albums, newest import first, narrowed by a
//! search, an import date, or what needs attention (spare copies, albums
//! not where the rules file them). Albums ticked in it are acted on
//! together: every action is previewed album by album first, and applying
//! applies what the preview showed, in the background, reporting each album.
//!
//! Every action names albums by exact path (`path:=…`), so it touches only
//! what the person was shown, and goes through the same `Library` calls as
//! the GraphQL mutations.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, patch, sse};
use crate::art::{AlbumArt, ArtRemoved, ArtSet, Image, Target};
use crate::library::{
    AlbumEdit, AlbumMove, CreditWrite, LibraryAlbum, Overview, Preview, TagEdits, by_path,
    release_id,
};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/library", get(view))
        .route("/library/find", post(find))
        .route("/library/album", get(album))
        .route("/library/album/releases", get(releases))
        .route("/library/album/edit", get(edit_form))
        .route("/library/album/cover", get(cover))
        .route(
            "/library/art/preview",
            post(art_preview).layer(axum::extract::DefaultBodyLimit::max(
                crate::art::MAX_BYTES + (1 << 20),
            )),
        )
        .route("/library/art/pending/{token}", get(pending_art))
        .route("/library/edit", post(batch_form))
        .route("/library/preview/{action}", post(preview))
        .route("/library/apply/{action}", post(apply))
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
    pub(super) fn of(m: &AlbumMove) -> Self {
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
pub(super) fn without_years(s: &str) -> String {
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
#[derive(Clone, Default, Debug, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(super) struct Filter {
    #[serde(deserialize_with = "trimmed")]
    pub(super) q: String,
    /// `today`, `week`, `month`, or `date` with `from` (`YYYY-MM-DD`).
    #[serde(deserialize_with = "trimmed")]
    pub(super) since: String,
    #[serde(deserialize_with = "trimmed")]
    pub(super) from: String,
    /// Empty for every album, `dups`, `formats` or `moves`.
    #[serde(deserialize_with = "trimmed")]
    pub(super) show: String,
    /// The album open in detail, by path; it does not narrow the list.
    #[serde(deserialize_with = "trimmed")]
    pub(super) album: String,
}

fn trimmed<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = <String as serde::Deserialize>::deserialize(d)?;
    Ok(s.trim().to_string())
}

/// The album an action works on, with the list it was opened from.
#[derive(serde::Deserialize)]
pub(super) struct AlbumQuery {
    #[serde(flatten)]
    pub(super) filter: Filter,
    #[serde(default)]
    pub(super) path: String,
}

impl Filter {
    /// The filter a posted form carries alongside its other fields.
    pub(super) fn from_fields(f: &[(String, String)]) -> Self {
        use serde::Deserialize as _;
        use serde::de::value::{Error, MapDeserializer};
        let pairs = f.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        Self::deserialize(MapDeserializer::<_, Error>::new(pairs)).unwrap_or_default()
    }

    /// The earliest import time the filter admits, in seconds. Days,
    /// weeks (from Monday) and months start at midnight where `now` is.
    pub(super) fn since_secs<Tz: chrono::TimeZone>(
        &self,
        now: chrono::DateTime<Tz>,
    ) -> Option<i64> {
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
    pub(super) fn terms(&self) -> Vec<String> {
        let mut terms: Vec<String> = self.q.split_whitespace().map(String::from).collect();
        if let Some(t) = self.since_secs(chrono::Local::now()) {
            terms.push(format!("added:{t}.."));
        }
        terms.extend(["added-", "albumartist+", "album+"].map(String::from));
        terms
    }

    pub(super) fn url(&self) -> String {
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
    let formats = match filter.show.as_str() {
        "formats" => in_formats(&s.app.library.albums(&[]).await?),
        _ => HashSet::new(),
    };
    Ok(rows(s, &filter.terms())
        .await?
        .into_iter()
        .filter(|r| match filter.show.as_str() {
            "dups" => r.marks.as_ref().is_some_and(|m| m.dup.is_some()),
            "formats" => formats.contains(&r.album.path),
            "moves" => r.refile().is_some(),
            _ => true,
        })
        .collect())
}

/// The paths of albums whose artist and title (ignoring edition suffixes)
/// the library holds in more than one format: a lossy copy left behind by a
/// lossless upgrade, which `duplicates` misses when the track lists differ.
pub(super) fn in_formats(albums: &[LibraryAlbum]) -> HashSet<String> {
    use sift::matching::{base_title, normalise};
    let mut groups: HashMap<(String, String), Vec<&LibraryAlbum>> = HashMap::new();
    for a in albums {
        let (Some(artist), Some(title)) = (&a.album_artist, &a.album) else {
            continue;
        };
        groups
            .entry((normalise(artist), normalise(&base_title(title))))
            .or_default()
            .push(a);
    }
    groups
        .into_values()
        .filter(|g| g.iter().map(|a| &a.format).collect::<HashSet<_>>().len() > 1)
        .flatten()
        .map(|a| a.path.clone())
        .collect()
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

/// Which album is open in detail, for every form that acts on the page to
/// send: the list is not redrawn when one opens or closes.
fn opened(path: &str) -> axum::response::sse::Event {
    super::signals(&serde_json::json!({ "_libalbum": path }).to_string())
}

#[derive(Template)]
#[template(path = "pages/library.html")]
struct Page {
    q: String,
    run: String,
}

/// The page around the list, which it fetches once drawn.
pub(super) fn page(s: &UiState, filter: &Filter) -> String {
    Page {
        q: filter.q.clone(),
        run: run_html(s),
    }
    .render()
    .unwrap_or_default()
}

/// The list the address describes.
async fn view(State(s): State<UiState>, uri: http::Uri) -> Response {
    let filter: Filter = super::query(&uri);
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="library"><p class="text-meta text-muted">Reading the library…</p></div>"#));
        yield Ok(patch(&list_html(&s, &filter).await));
        yield Ok(unselected());
        if !filter.album.is_empty() {
            yield Ok(patch(&album_html(&s, &filter).await));
        }
        yield Ok(opened(&filter.album));
        yield Ok(super::place(&filter.url()));
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
        opened(""),
        super::place(
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
    art: Option<AlbumArt>,
    /// Where its cover is drawn from, when it has one.
    cover: Option<String>,
}

impl AlbumView<'_> {
    /// The pictures embedded in its files, each kind once, with how many
    /// files carry it.
    fn embedded(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for f in self.art.iter().flat_map(|a| &a.files) {
            for p in &f.pictures {
                let desc = picture(p);
                match out.iter_mut().find(|(d, _)| *d == desc) {
                    Some((_, n)) => *n += 1,
                    None => out.push((desc, 1)),
                }
            }
        }
        out
    }
    /// Files whose pictures could not be read.
    fn unreadable(&self) -> usize {
        self.art
            .iter()
            .flat_map(|a| &a.files)
            .filter(|f| f.error.is_some())
            .count()
    }
    fn cover_file(&self, c: &crate::art::CoverFile) -> String {
        let size = match (c.width, c.height) {
            (Some(w), Some(h)) => format!(", {w}×{h}"),
            _ => String::new(),
        };
        format!("{}{size}, {}", c.name, super::human(c.bytes))
    }
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
            art: s.app.library.art(&row.album.path).await.ok(),
            cover: cover_url(&s.app.library, &row.album.path).await,
            tracks,
        }
        .render()
        .unwrap_or_default(),
        Err(e) => format!(
            r#"<div id="lib-album" class="card"><p class="text-meta text-bad wrap-anywhere">{}</p></div>"#,
            escape(&format!("{e:#}"))
        ),
    }
}

/// One album open in detail, above the list.
async fn album(State(s): State<UiState>, uri: http::Uri) -> Response {
    let filter: Filter = super::query(&uri);
    super::many(vec![
        patch(&album_html(&s, &filter).await),
        patch(r#"<div id="lib-preview"></div>"#),
        opened(&filter.album),
        super::place(&filter.url()),
    ])
}

/// The address of the album's cover, when it has one, versioned by the
/// file it comes from so a browser keeps it until the art changes.
async fn cover_url(library: &crate::library::Library, path: &str) -> Option<String> {
    let (_, source) = library.cover(path).await.ok()??;
    let meta = std::fs::metadata(&source).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let version = key(&format!("{}:{}:{modified}", source.display(), meta.len()));
    Some(super::place_url(
        "/library/album/cover",
        &[("path", path), ("v", &version)],
    ))
}

#[derive(serde::Deserialize)]
struct CoverQuery {
    path: String,
    #[serde(default)]
    v: String,
}

/// An album's cover: its leading cover image, or the front cover embedded
/// in its first file. Only albums the library's index holds are served. A
/// versioned address names one version of the art, so it is kept for good.
async fn cover(State(s): State<UiState>, Query(q): Query<CoverQuery>) -> Response {
    let Ok(Some((bytes, _))) = s.app.library.cover(&q.path).await else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    image_response(bytes, !q.v.is_empty())
}

pub(super) fn image_response(bytes: Vec<u8>, immutable: bool) -> Response {
    let Some(mime) = crate::art::mime_of(&bytes) else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    let cache = if immutable {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    (
        [
            (http::header::CONTENT_TYPE, mime),
            (http::header::CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response()
}

/// Album art previewed and not yet applied, by token: what applying writes
/// is the image the preview showed, not a second fetch of its URL.
pub(super) type ArtStash = std::sync::Arc<
    parking_lot::Mutex<HashMap<String, (std::time::Instant, std::sync::Arc<Image>)>>,
>;

/// How long a previewed image waits to be applied, and how many wait.
const ART_KEPT: std::time::Duration = std::time::Duration::from_secs(3600);
pub(super) const ART_HELD: usize = 4;

pub(super) fn stash(art: &ArtStash, image: Image) -> String {
    let mut held = art.lock();
    held.retain(|_, (at, _)| at.elapsed() < ART_KEPT);
    while held.len() >= ART_HELD {
        let Some(oldest) = held
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        held.remove(&oldest);
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    held.insert(
        token.clone(),
        (std::time::Instant::now(), std::sync::Arc::new(image)),
    );
    token
}

pub(super) fn stashed(art: &ArtStash, token: &str) -> Option<std::sync::Arc<Image>> {
    art.lock()
        .get(token)
        .filter(|(at, _)| at.elapsed() < ART_KEPT)
        .map(|(_, i)| i.clone())
}

/// An image previewed as an album's new art.
async fn pending_art(State(s): State<UiState>, Path(token): Path<String>) -> Response {
    match stashed(&s.art, &token) {
        Some(image) => image_response(image.bytes.clone(), true),
        None => http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// New art for the album open in detail, from an uploaded image or a URL:
/// read and checked, kept for applying, and previewed as any other action.
async fn art_preview(
    State(s): State<UiState>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let mut form = Vec::new();
    let mut upload = Vec::new();
    let read: anyhow::Result<()> = async {
        while let Some(field) = multipart.next_field().await? {
            let name = field.name().unwrap_or_default().to_string();
            if name == "image" {
                upload = field.bytes().await?.to_vec();
            } else {
                let value = field.text().await?;
                form.push((name, value));
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = read {
        return failed(&e);
    }
    let url = super::field(&form, "url")
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let image = if !upload.is_empty() {
        s.app.library.prepare_art(&upload)
    } else if !url.is_empty() {
        s.app.library.fetch_art(&url).await
    } else {
        Err(anyhow::anyhow!("choose an image file or give its URL"))
    };
    match image {
        Ok(image) => {
            let token = stash(&s.art, image);
            form.push(("art".into(), token));
            previewed(s, Action::SetArt, form).await
        }
        Err(e) => failed(&e),
    }
}

/// A picture embedded in a file, as a person reads it.
fn picture(p: &crate::art::Picture) -> String {
    let kind = match p.kind.as_str() {
        "CoverFront" => "front cover",
        "CoverBack" => "back cover",
        "Other" => "other",
        k => k,
    };
    let mut parts = vec![kind.to_string()];
    if let Some(m) = &p.mime {
        parts.push(m.trim_start_matches("image/").to_uppercase());
    }
    if let (Some(w), Some(h)) = (p.width, p.height) {
        parts.push(format!("{w}×{h}"));
    }
    parts.push(super::human(p.bytes as u64));
    parts.join(", ")
}

#[derive(Template)]
#[template(path = "library_releases.html")]
struct ReleasesView<'a> {
    path: &'a str,
    filter: &'a Filter,
    candidates: Vec<sift::Candidate>,
    review: Option<String>,
    error: Option<String>,
    discogs: bool,
}

/// The releases MusicBrainz (and Discogs, when enabled) has for an album's
/// files, closest first, to re-tag it from; or one pasted.
async fn releases(
    State(s): State<UiState>,
    Query(AlbumQuery { filter, path }): Query<AlbumQuery>,
) -> Response {
    sse(async_stream::stream! {
        yield Ok(patch(r#"<div id="lib-retag"><p class="text-meta text-muted">Asking for releases that fit these files…</p></div>"#));
        let discogs = s.app.jobs.discogs();
        let view = match s.app.library.rematch(&path).await {
            Ok(r) => ReleasesView { path: &path, filter: &filter, candidates: r.candidates, review: r.review, error: None, discogs },
            Err(e) => ReleasesView { path: &path, filter: &filter, candidates: Vec::new(), review: None, error: Some(format!("{e:#}")), discogs },
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
async fn edit_form(
    State(s): State<UiState>,
    Query(AlbumQuery { filter, path }): Query<AlbumQuery>,
) -> Response {
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

#[derive(Template)]
#[template(path = "library_batch.html")]
struct BatchView<'a> {
    filter: &'a Filter,
    rows: &'a [Row],
}

impl BatchView<'_> {
    fn fields(&self) -> Vec<(&'static str, &'static str)> {
        batch_fields().collect()
    }
}

/// Album-wide fields for the albums ticked in the list, set on all of them.
async fn batch_form(State(s): State<UiState>, body: Bytes) -> Response {
    let (filter, rows) = match selection(&s, &fields(&body)).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    if rows.is_empty() {
        return failed(&anyhow::anyhow!("nothing selected"));
    }
    super::one(
        BatchView {
            filter: &filter,
            rows: &rows,
        }
        .render()
        .unwrap_or_default(),
    )
}

/// What can be done to the albums ticked in the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    Refile,
    Credit,
    Enrich,
    Reimport,
    Bin,
    /// Tags typed by hand: any field of one album's files, or album-wide
    /// fields across several.
    Edit,
    /// Every picture embedded in the files taken out.
    RemoveArt,
    /// An image chosen for one album, as its art.
    SetArt,
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
            "remove-art" => Self::RemoveArt,
            "set-art" => Self::SetArt,
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
            Self::RemoveArt => "remove-art",
            Self::SetArt => "set-art",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Refile => "Re-file",
            Self::Credit => "Credit artists",
            Self::Enrich => "Enrich",
            Self::Reimport => "Re-tag",
            Self::Bin => "Bin",
            Self::Edit => "Edit tags",
            Self::RemoveArt => "Remove art",
            Self::SetArt => "Set art",
        }
    }

    /// The apply button's words, before the count.
    fn verb(self) -> &'static str {
        match self {
            Self::Credit => "Credit artists on",
            Self::Bin => "Bin",
            Self::Edit => "Edit",
            Self::RemoveArt => "Remove art from",
            Self::SetArt => "Set art on",
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
            Self::RemoveArt => "Removing art from",
            Self::SetArt => "Setting art on",
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
            Self::RemoveArt => "Removed art from",
            Self::SetArt => "Set art on",
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
                "The fields typed, as each file has them and as they would be, and where the new tags file the album. The albums ticked are edited together: if any file cannot be written, none keep the edit."
            }
            Self::RemoveArt => {
                "Every picture embedded in each file is taken out; the rest of the tags stay. Cover images in the folder are left, and players show those first."
            }
            Self::SetArt => {
                "The image becomes the only picture embedded in each file, as an import embeds art, scaled down past the largest width imports embed. A cover file is written beside them when asked for, and cover images already there move to the bin."
            }
            Self::Bin => {
                "Each album moves to the bin beside the library, out of sight of the shares and the player. Nothing is deleted. Spare copies are ticked; kept and only copies are left for you to tick."
            }
        }
    }

    /// How many albums one preview works out, for the actions that ask
    /// MusicBrainz or LRCLIB, seconds an album. The rest are local and
    /// preview every album selected.
    fn cap(self) -> Option<usize> {
        match self {
            Self::Refile | Self::Bin | Self::Edit | Self::SetArt => None,
            Self::Credit | Self::Enrich | Self::Reimport | Self::RemoveArt => Some(50),
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
    /// The tags typed for each album, by path, and its files as they were
    /// read to compare them.
    edits: HashMap<String, (TagEdits, Vec<sift::meta::Track>)>,
    /// The image previewed as new art, and where it goes.
    art: Option<(ArtChoice, std::sync::Arc<Image>)>,
}

/// New art as a preview hands it to applying: the image kept by token, and
/// where it goes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct ArtChoice {
    pub(super) token: String,
    pub(super) embed: bool,
    pub(super) file: bool,
}

impl ArtChoice {
    /// The choice the art form makes: embedding unless only a cover file
    /// is ticked, as imports embed.
    pub(super) fn from_form(form: &[(String, String)]) -> Option<Self> {
        let ticked = |k| super::field(form, k).is_some_and(|v| !v.is_empty());
        Some(Self {
            token: super::field(form, "art")?.to_string(),
            embed: ticked("embed"),
            file: ticked("file"),
        })
    }

    fn target(&self) -> Target {
        Target {
            embed: self.embed,
            file: self.file,
        }
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
        release: Box<sift::Candidate>,
        files: Vec<FileChanges>,
        missing: Vec<String>,
        untouched: Vec<String>,
        /// Every file's front cover is replaced.
        cover: bool,
        companions: Vec<Companion>,
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
    RemoveArt(ArtRemoved),
    SetArt {
        set: ArtSet,
        token: String,
    },
}

/// A file beside the tracks that follows the album to its new directory,
/// or stays because something is already there.
struct Companion {
    file: String,
    to: String,
    stays: bool,
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

pub(super) struct Lyric {
    pub(super) file: String,
    pub(super) synced: bool,
    pub(super) lines: usize,
    pub(super) first: String,
}

impl Lyric {
    pub(super) fn of(l: &sift::PlannedLyrics) -> Self {
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
    /// The separator credits are split on, carried to applying.
    split: &'a str,
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
    fn label<'b>(&self, field: &'b str) -> &'b str {
        field_label(field)
    }
}

/// A tag field in a list of changes, by the name the album's tag table
/// and the edit form give it.
fn field_label(field: &str) -> &str {
    match field {
        "title" => "title",
        "artist" => "artist",
        "artists" => "artists",
        "album" => "album",
        "albumartist" => "album artist",
        "albumartists" => "album artists",
        "track" => "track",
        "tracktotal" => "tracks",
        "disc" => "disc",
        "disctotal" => "discs",
        "date" | "year" => "date",
        "original_date" => "original date",
        "genre" => "genre",
        "label" => "label",
        "catalognum" => "catalogue number",
        "country" => "country",
        "media" => "media",
        "comp" => "compilation",
        "mb_albumid" => "MusicBrainz release",
        "mb_releasegroupid" => "MusicBrainz release group",
        "mb_trackid" => "MusicBrainz recording",
        "mb_releasetrackid" => "MusicBrainz track",
        "mb_artistids" => "MusicBrainz artists",
        "mb_albumartistids" => "MusicBrainz album artists",
        other => other,
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
                            "Nothing to credit: already credited, one artist, or a credit neither a release nor the library's artists split. Type the separator its artists are joined by to split on it."
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
                let changes = plan.files.iter().filter(|f| !f.changes.is_empty() || f.to.as_ref().is_some_and(|to| *to != f.path)).count();
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
                    release: Box::new(c.release.clone()),
                    files: plan
                        .files
                        .into_iter()
                        .map(|f| {
                            let to = f.to.as_deref().unwrap_or(&f.path);
                            FileChanges::of(&root, &f.path, to, f.changes)
                        })
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
                    cover: plan.cover,
                    companions: plan
                        .companions
                        .iter()
                        .map(|c| Companion {
                            file: c.from.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                            to: rel(&root, &c.to.to_string_lossy()).to_string(),
                            stays: c.stays,
                        })
                        .collect(),
                };
            }
            Action::Edit => {
                let Some((edits, tracks)) = ask.edits.get(&path) else {
                    p.note = Some("No tags typed.".into());
                    return Ok(());
                };
                let plan = library.edit_plan(&path, tracks, edits).await?;
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
            Action::RemoveArt => {
                let r = library
                    .remove_art(std::slice::from_ref(&path), false)
                    .await?
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("no longer in the library"))?;
                p.applicable = !r.files.is_empty();
                p.ticked = p.applicable;
                p.note = if !p.applicable {
                    Some("No file carries embedded art.".into())
                } else if !r.cover_files.is_empty() {
                    Some(format!(
                        "Players show the cover image in the folder, which stays: {}.",
                        r.cover_files.join(", ")
                    ))
                } else {
                    None
                };
                p.body = Body::RemoveArt(r);
            }
            Action::SetArt => {
                let Some((choice, image)) = &ask.art else {
                    p.note = Some("No image chosen.".into());
                    return Ok(());
                };
                let set = library.set_art(&path, image, choice.target(), false).await?;
                p.applicable = true;
                p.ticked = true;
                p.note = (!set.kept.is_empty()).then(|| {
                    format!(
                        "Players show the cover image in the folder before embedded art, and it stays: {}. Tick “Cover file” to replace it.",
                        set.kept.join(", ")
                    )
                });
                p.with = serde_json::to_string(choice).ok();
                p.body = Body::SetArt {
                    set,
                    token: choice.token.clone(),
                };
            }
            Action::Bin => {
                let dup = row.marks.as_ref().and_then(|m| m.dup.as_ref());
                match library.bin_destination(&path) {
                    Ok(to) => {
                        p.applicable = true;
                        p.body = Body::Bin {
                            to: to.to_string_lossy().into_owned(),
                            keep: dup.map(|d| d.keep.clone()).unwrap_or_default(),
                            reason: dup.and_then(|d| d.spare.clone()).unwrap_or_default(),
                        };
                        match dup {
                            Some(Dup {
                                keep,
                                spare: Some(_),
                            }) => {
                                p.ticked = true;
                                p.with = Some(keep.clone());
                            }
                            Some(_) => {
                                p.note = Some(
                                    "The copy kept: binning it leaves only the spare.".into(),
                                )
                            }
                            None => {
                                p.note = Some(
                                    "The only copy: binning takes it out of the shares and the player."
                                        .into(),
                                )
                            }
                        }
                    }
                    Err(why) => p.note = Some(format!("Cannot be binned: {why}.")),
                }
            }
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
    previewed(s, action, fields(&body)).await
}

async fn previewed(s: UiState, action: Action, form: Vec<(String, String)>) -> Response {
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
        let split = super::field(&form, "split").unwrap_or_default().trim().to_string();
        let shell = PlanView { action, filter: &filter, selected, shown, split: &split };
        yield Ok(patch(&shell.render().unwrap_or_default()));
        let known = if matches!(action, Action::Credit | Action::Enrich) {
            match s.app.library.known_artists().await {
                Ok(k) => k.splitting_on(&split),
                Err(e) => {
                    yield Ok(patch(&super::flash_err(&format!("{e:#}"))));
                    return;
                }
            }
        } else {
            Default::default()
        };
        let mut applicable = 0;
        // Where each edited album would be filed: the second headed for a
        // folder is refused when the batch is re-filed.
        let mut filed: HashMap<String, String> = HashMap::new();
        for (i, row) in rows.iter().take(shown).enumerate() {
            yield Ok(patch(&format!(
                r#"<p id="pv-progress" class="text-meta text-muted">Working out {} of {shown}: {}</p>"#,
                i + 1,
                escape(&title_of(&row.album)),
            )));
            let mut p = plan_one(&s, action, row, &known, &ask).await;
            if let Body::Edit { into: into @ Some(_), .. } = &mut p.body {
                let d = into.clone().unwrap_or_default();
                match filed.get(&d) {
                    Some(first) => {
                        *into = None;
                        p.note = Some(format!("The tags change, but the album stays where it is: {first} is filed into {d} first."));
                    }
                    None => {
                        filed.insert(d, p.title.clone());
                    }
                }
            }
            applicable += usize::from(p.applicable);
            let html = PlannedView { p: &p, root: &root }.render().unwrap_or_default();
            yield Ok(super::append("#pv-albums", &html));
        }
        yield Ok(patch(&format!(
            r#"<p id="pv-progress" class="text-meta text-muted">{applicable} of {shown} would change.</p>"#
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
            r#"<div id="pv-apply" class="mt-2.5 flex flex-wrap items-center gap-2" data-init="$_apply = document.querySelectorAll('#lib-apply input[name=path]:checked').length">{button}<button type="button" class="quiet" data-on:click="document.getElementById('lib-preview').replaceChildren()">Cancel</button></div>"#
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

/// The album-wide fields offered for several albums at once: all but the
/// album's title, which no two albums share.
fn batch_fields() -> impl Iterator<Item = (&'static str, &'static str)> {
    ALBUM_FIELDS.iter().copied().filter(|(f, _)| *f != "album")
}

/// Fields the edit form offers on each file.
const TRACK_FIELDS: &[&str] = &["disc", "track", "title", "artist"];

/// Of those, the ones that hold a number.
const NUMBERS: &[&str] = &["disc", "track"];

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
/// agree on clears it. A number typed as anything but digits is refused,
/// rather than written as no number at all.
pub(super) fn edits_from(
    tracks: &[sift::meta::Track],
    form: &[(String, String)],
) -> anyhow::Result<TagEdits> {
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
        if (varies && typed.is_empty()) || (!varies && typed == now.unwrap_or_default().trim()) {
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
            if typed != t.field(field).unwrap_or_default().trim() {
                if NUMBERS.contains(field) && !typed.is_empty() && typed.parse::<u32>().is_err() {
                    anyhow::bail!(
                        "{field} for {} must be a number, not {typed:?}",
                        t.path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                changes.push((field.to_string(), (!typed.is_empty()).then_some(typed)));
            }
        }
        if !changes.is_empty() {
            edits.tracks.push((t.path.clone(), changes));
        }
    }
    edits.clear_stale_credits(tracks);
    Ok(edits)
}

/// The edits a form for several albums asks of one of them, against its
/// files as they are now. A field typed is set wherever a file has it
/// otherwise; a field left blank is left alone, and one ticked to clear is
/// cleared wherever a file has it.
pub(super) fn batch_edits_from(
    tracks: &[sift::meta::Track],
    form: &[(String, String)],
) -> anyhow::Result<TagEdits> {
    let mut edits = TagEdits::default();
    let mut asked = false;
    for (field, label) in batch_fields() {
        let typed = super::field(form, &format!("a-{field}"))
            .map(str::trim)
            .unwrap_or_default();
        let clear = super::field(form, &format!("clear-{field}")).is_some();
        let value = match (typed.is_empty(), clear) {
            (true, false) => continue,
            (false, true) => anyhow::bail!("{label}: type a value or tick Clear, not both"),
            (true, true) => None,
            (false, false) => Some(typed.to_string()),
        };
        asked = true;
        if tracks
            .iter()
            .all(|t| t.field(field).map(|v| v.trim().to_string()) == value)
        {
            continue;
        }
        edits.album.push((field.to_string(), value));
    }
    if !asked {
        anyhow::bail!("Type a value or tick Clear for at least one field.");
    }
    edits.clear_stale_credits(tracks);
    Ok(edits)
}

/// What the form chose: a release (picked or pasted) for re-tagging a
/// single album, or typed tags for an edit, from the album's form or the
/// form for several.
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
                    anyhow::anyhow!("{p:?} is not a MusicBrainz or Discogs release id or link")
                })?),
                None => super::field(form, "release").map(str::to_string),
            };
            if let Some(r) = &release {
                s.app.jobs.check_release(r)?;
                one()?;
            }
            Ok(Ask {
                release,
                ..Default::default()
            })
        }
        Action::Edit if super::field(form, "batch").is_some() => {
            let mut edits = HashMap::new();
            for row in rows {
                let path = &row.album.path;
                let tracks = s.app.library.tracks(path).await?;
                edits.insert(path.clone(), (batch_edits_from(&tracks, form)?, tracks));
            }
            Ok(Ask {
                edits,
                ..Default::default()
            })
        }
        Action::Edit => {
            let path = one()?;
            let tracks = s.app.library.tracks(path).await?;
            Ok(Ask {
                edits: HashMap::from([(path.to_string(), (edits_from(&tracks, form)?, tracks))]),
                ..Default::default()
            })
        }
        Action::SetArt => {
            one()?;
            let choice = ArtChoice::from_form(form)
                .ok_or_else(|| anyhow::anyhow!("choose an image file or give its URL"))?;
            if !choice.embed && !choice.file {
                anyhow::bail!("tick where the art goes: embedded, a cover file, or both");
            }
            let image = stashed(&s.art, &choice.token)
                .ok_or_else(|| anyhow::anyhow!("that image is no longer held; choose it again"))?;
            Ok(Ask {
                art: Some((choice, image)),
                ..Default::default()
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
    /// Counts runs, so a page following one knows when another starts.
    id: u64,
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

impl LibraryRun {
    fn verb(&self) -> &'static str {
        match self.action {
            Some(a) if self.running => a.doing(),
            Some(a) => a.done(),
            None => "",
        }
    }
    fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.ok).count()
    }
}

#[derive(Template)]
#[template(path = "library_run.html")]
struct RunView<'a> {
    run: &'a LibraryRun,
    root: &'a str,
}

impl RunView<'_> {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(self.root, p)
    }
}

#[derive(Template)]
#[template(path = "library_run_head.html")]
struct RunHead<'a> {
    run: &'a LibraryRun,
    root: &'a str,
}

impl RunHead<'_> {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(self.root, p)
    }
}

#[derive(Template)]
#[template(path = "library_run_row.html")]
struct RunRow<'a> {
    r: &'a Outcome,
    root: &'a str,
}

impl RunRow<'_> {
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(self.root, p)
    }
}

/// What a page's stream last sent of the run panel.
#[derive(Default)]
pub(super) struct RunSent {
    stamp: Option<(u64, bool, usize, usize, Option<String>, bool)>,
    rows: usize,
}

/// The run panel's changes since `sent`: the whole panel when a run
/// starts or ends, and while it runs, its head and the albums done since.
pub(super) fn run_events(s: &UiState, sent: &mut RunSent) -> Vec<axum::response::sse::Event> {
    let run = s.run.lock();
    let stamp = (
        run.id,
        run.running,
        run.done,
        run.total,
        run.current.clone(),
        run.error.is_some(),
    );
    if sent.stamp.as_ref() == Some(&stamp) {
        return Vec::new();
    }
    let root = s.app.library.root().to_string_lossy().into_owned();
    let following = run.running
        && sent.rows <= run.results.len()
        && sent
            .stamp
            .as_ref()
            .is_some_and(|(id, running, ..)| *id == run.id && *running);
    let events = if following {
        let head = RunHead {
            run: &run,
            root: &root,
        };
        let mut events = vec![patch(&head.render().unwrap_or_default())];
        let new: String = run.results[sent.rows..]
            .iter()
            .rev()
            .map(|r| RunRow { r, root: &root }.render().unwrap_or_default())
            .collect();
        if !new.is_empty() {
            events.push(super::prepend("#lib-run-rows", &new));
        }
        events
    } else {
        vec![patch(
            &RunView {
                run: &run,
                root: &root,
            }
            .render()
            .unwrap_or_default(),
        )]
    };
    sent.stamp = Some(stamp);
    sent.rows = run.results.len();
    events
}

pub(super) fn run_html(s: &UiState) -> String {
    let run = s.run.lock();
    RunView {
        run: &run,
        root: &s.app.library.root().to_string_lossy(),
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
            id: run.id + 1,
            action: Some(action),
            total: paths.len(),
            running: true,
            ..Default::default()
        };
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (app, state, art) = (s.app.clone(), s.run.clone(), s.art.clone());
    let split = super::field(&form, "split").unwrap_or_default().to_string();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        // A panic in the run still frees the slot.
        match std::panic::AssertUnwindSafe(run(&app, &state, &art, action, &paths, &with, &split))
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
        // The album open in detail is drawn again where it now is, or
        // closed when it has left the library (binned).
        let mut filter = filter;
        let was_open = !filter.album.is_empty();
        if was_open {
            let moved = s.run.lock().results.iter().find(|r| r.path == filter.album).and_then(|r| r.to.clone());
            if let Some(to) = moved {
                filter.album = to;
            }
            let gone = rows(&s, &by_path([filter.album.as_str()])).await.is_ok_and(|r| r.is_empty());
            if gone {
                filter.album.clear();
            }
        }
        yield Ok(patch(&list_html(&s, &filter).await));
        yield Ok(unselected());
        if was_open {
            yield Ok(patch(&if filter.album.is_empty() {
                r#"<div id="lib-album"></div>"#.to_string()
            } else {
                album_html(&s, &filter).await
            }));
            yield Ok(opened(&filter.album));
            yield Ok(super::place(&filter.url()));
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

/// An album whose tags were written where it is.
fn edited(path: String) -> Outcome {
    Outcome {
        path,
        to: None,
        ok: true,
        detail: "tags written in place".into(),
        problems: Vec::new(),
    }
}

async fn run(
    app: &crate::App,
    state: &parking_lot::Mutex<LibraryRun>,
    art: &ArtStash,
    action: Action,
    paths: &[String],
    with: &HashMap<String, String>,
    split: &str,
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
    if action == Action::Edit {
        // One call, so every album is checked before any is written and a
        // failed write puts back what was already written.
        let edits = paths
            .iter()
            .map(|path| {
                let edits: TagEdits = with
                    .get(&key(path))
                    .and_then(|e| serde_json::from_str(e).ok())
                    .ok_or_else(|| anyhow::anyhow!("no edits given for {path}"))?;
                Ok(AlbumEdit {
                    dir: path.clone(),
                    edits,
                    read: None,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>();
        state.lock().current = Some(match paths {
            [one] => one.clone(),
            _ => "all of them together".into(),
        });
        let edited_all = match edits {
            Ok(edits) => library.edit_many(&edits).await,
            Err(e) => Err(e),
        };
        match edited_all {
            Ok(moves) => {
                for m in moves {
                    record(match (m.to, m.refused) {
                        (Some(to), _) if to != m.from => Outcome {
                            detail: format!("→ {}", rel(&root, &to)),
                            to: Some(to),
                            ..edited(m.from)
                        },
                        (_, Some(why)) => Outcome {
                            detail: "tags written; left where it is".into(),
                            problems: vec![why.replace(&format!("{root}/"), "")],
                            ..edited(m.from)
                        },
                        _ => edited(m.from),
                    });
                }
            }
            Err(e) => {
                for path in paths {
                    record(Outcome {
                        path: path.clone(),
                        to: None,
                        ok: false,
                        detail: format!("{e:#}"),
                        problems: Vec::new(),
                    });
                }
            }
        }
        return Ok(());
    }
    let known = match action {
        Action::Credit | Action::Enrich => library.known_artists().await?.splitting_on(split),
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
                Action::Edit => unreachable!("edited above"),
                Action::RemoveArt => {
                    let r = library
                        .remove_art(std::slice::from_ref(path), true)
                        .await?
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("no longer in the library"))?;
                    (
                        format!("removed from {} files", r.files.len()),
                        Vec::new(),
                        None,
                    )
                }
                Action::SetArt => {
                    let choice: ArtChoice = attached
                        .and_then(|c| serde_json::from_str(c).ok())
                        .ok_or_else(|| anyhow::anyhow!("no image chosen"))?;
                    let image = stashed(art, &choice.token).ok_or_else(|| {
                        anyhow::anyhow!("that image is no longer held; choose it again")
                    })?;
                    let set = library.set_art(path, &image, choice.target(), true).await?;
                    let mut parts = Vec::new();
                    if !set.embedded.is_empty() {
                        parts.push(format!("embedded in {} files", set.embedded.len()));
                    }
                    if let Some(f) = &set.cover_file {
                        parts.push(format!("written as {f}"));
                    }
                    if !set.binned.is_empty() {
                        parts.push(format!("binned {}", set.binned.join(", ")));
                    }
                    (parts.join(" · "), Vec::new(), None)
                }
                Action::Bin => {
                    let Some(keep) = attached else {
                        let to = library.bin_album(path).await?;
                        return Ok((format!("→ {}", to.display()), Vec::new(), None));
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
