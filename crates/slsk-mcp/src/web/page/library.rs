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
use askama_datastar::{ElementPatchMode, Html, Signals};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use koan_kit_web as kit;

use super::{Body, Page, Tab, View};
use crate::art::{AlbumArt, Image};
use crate::library::{AlbumMove, LibraryAlbum, Overview, by_path};
use crate::web::stream::Shown;
use crate::web::{Draw, UiState, failed, field, fields, insert, place};

pub(in crate::web) mod plan;
pub(in crate::web) mod run;

pub(in crate::web) use plan::{Action, batch_form, edit_form, previewed, title_of};
pub(in crate::web) use run::{Runs, resume};

/// What a re-file would change, from the album's directory and the one the
/// rules give it. The first three only correct how the folder is written;
/// the fourth moves the album somewhere its folder does not already say.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(in crate::web) enum Change {
    FileNames,
    Spelling,
    Year,
    Tags,
    Refused,
}

impl Change {
    pub(in crate::web) fn of(m: &AlbumMove) -> Self {
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
pub(in crate::web) fn without_years(s: &str) -> String {
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
pub(in crate::web) fn key(path: &str) -> String {
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

/// When an album was imported: how long ago within the week, else the date.
fn imported(secs: Option<i64>) -> Option<kit::Time<'static>> {
    let t = chrono::DateTime::from_timestamp(secs?, 0)?;
    let time = kit::Time::seconds(t.timestamp());
    Some(if (chrono::Utc::now() - t).num_days() < 7 {
        time.relative()
    } else {
        time.date()
    })
}

/// What the list shows: a search, an import date, and which albums.
#[derive(Clone, Default, Debug, PartialEq, serde::Deserialize)]
#[serde(default)]
pub(in crate::web) struct Filter {
    #[serde(deserialize_with = "trimmed")]
    pub(in crate::web) q: String,
    /// `today`, `week`, `month`, or `date` with `from` (`YYYY-MM-DD`).
    #[serde(deserialize_with = "trimmed")]
    pub(in crate::web) since: String,
    #[serde(deserialize_with = "trimmed")]
    pub(in crate::web) from: String,
    /// Empty for every album, `dups`, `formats` or `moves`.
    #[serde(deserialize_with = "trimmed")]
    pub(in crate::web) show: String,
}

/// The albums open in detail, by path, as the address lists them: each
/// opens in its own row, so several can be worked on at once.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(in crate::web) struct Open {
    album: Vec<String>,
}

impl Open {
    pub(in crate::web) fn albums(self) -> Vec<String> {
        self.album.into_iter().filter(|a| !a.is_empty()).collect()
    }
}

/// The element an album's detail opens into, in its row; everything the
/// detail holds is named after it.
pub(in crate::web) fn slot_of(path: &str) -> String {
    format!("a{}", key(path))
}

/// The slot a form names, or `lib` (the preview above the list) when it
/// names none or something that could not be one.
fn slot_in(form: &[(String, String)]) -> String {
    field(form, "slot")
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("lib")
        .to_string()
}

fn trimmed<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = <String as serde::Deserialize>::deserialize(d)?;
    Ok(s.trim().to_string())
}

/// The album an action works on, with the list it was opened from.
#[derive(serde::Deserialize)]
pub(in crate::web) struct AlbumQuery {
    #[serde(flatten)]
    pub(in crate::web) filter: Filter,
    #[serde(default)]
    pub(in crate::web) path: String,
}

impl Filter {
    /// The filter a posted form carries alongside its other fields.
    pub(in crate::web) fn from_fields(f: &[(String, String)]) -> Self {
        use serde::Deserialize as _;
        use serde::de::value::{Error, MapDeserializer};
        let pairs = f.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        Self::deserialize(MapDeserializer::<_, Error>::new(pairs)).unwrap_or_default()
    }

    /// The earliest import time the filter admits, in seconds. Days,
    /// weeks (from Monday) and months start at midnight where `now` is.
    pub(in crate::web) fn since_secs<Tz: chrono::TimeZone>(
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
    pub(in crate::web) fn terms(&self) -> Vec<String> {
        let mut terms: Vec<String> = self.q.split_whitespace().map(String::from).collect();
        if let Some(t) = self.since_secs(chrono::Local::now()) {
            terms.push(format!("added:{t}.."));
        }
        terms.extend(["added-", "albumartist+", "album+"].map(String::from));
        terms
    }

    /// The list's address with `open` open in their rows.
    pub(in crate::web) fn url_with(&self, open: &[String]) -> String {
        let mut params = self.params();
        params.extend(open.iter().map(|p| ("album", p.as_str())));
        crate::web::place_url("/library", &params)
    }

    /// The filter as address parameters.
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
        ]
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
pub(in crate::web) struct Dup {
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

pub(in crate::web) struct Row {
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
pub(in crate::web) fn in_formats(albums: &[LibraryAlbum]) -> HashSet<String> {
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
    let overview = library.overview_soon().await?;
    let marks = marks(&overview);
    Ok(albums
        .into_iter()
        .map(|album| Row {
            marks: marks.get(&album.path).cloned(),
            album,
        })
        .collect())
}

/// Albums drawn at first, and added by each "Show more". Every row carries
/// its checkbox and link, and a browser drawing a whole large library at
/// once stalls for seconds.
const SHOWN: usize = 200;

pub(in crate::web) struct Library;

#[derive(Template)]
#[template(path = "library/page.html")]
struct LibraryPage {
    q: String,
    /// The date field is in use, so it starts shown.
    dated: bool,
    run: Html,
}

impl LibraryPage {
    /// The date imports are found since: shown only while "imported since…" is chosen.
    fn from(&self) -> kit::Input<'static> {
        let input = kit::Input::new(kit::InputKind::Date, "from")
            .label("Imported since")
            .bind("_libfrom")
            .datastar("attr:hidden", "$_libsince != 'date'")
            .datastar("on:change", "el.form.requestSubmit()")
            .class("flex-auto");
        if self.dated {
            input
        } else {
            input.attr("hidden", "")
        }
    }
}

impl Page for Library {
    const TAB: Tab = Tab::Library;
    const STREAM: &'static str = "/stream/library";
    type Args = View<Filter>;

    /// The page around the list. Reading the library takes a moment, so the
    /// stream draws the list, and the albums the address opens in it.
    async fn body(s: &UiState, View(filter): View<Filter>) -> anyhow::Result<Body> {
        let run = {
            let run = s.library.run.read();
            run::RunPanel {
                run: &run,
                root: &s.app.library.root().to_string_lossy(),
            }
            .draw()
        };
        Ok(Body::of(&LibraryPage {
            q: filter.q.clone(),
            dated: filter.since == "date",
            run,
        })
        .signal("_libq", filter.q.clone())
        .signal("_libsince", filter.since.clone())
        .signal("_libfrom", filter.from.clone())
        .signal("_libshow", filter.show.clone())
        // The slot of the album whose art form is open, if any.
        .signal("_artform", "")
        // The page has no list yet: the stream draws it.
        .signal("libseen", 0)
        .signal("librows", 0))
    }
}

/// The albums the filter admits, newest import first, the first page of them.
#[derive(Template)]
#[template(path = "library/list.html")]
pub(in crate::web) struct List {
    filter: Filter,
    rows: Option<Rows>,
    total: usize,
    /// Spare copies among everything the filter admits.
    spares: usize,
    error: Option<String>,
}

impl List {
    /// The albums the list draws, by path.
    pub(in crate::web) fn paths(&self) -> impl Iterator<Item = &str> {
        self.rows
            .iter()
            .flat_map(|r| &r.rows)
            .map(|r| r.album.path.as_str())
    }

    /// The list with its first `held` rows, a page at least.
    pub(in crate::web) async fn of(s: &UiState, filter: &Filter, held: usize) -> Self {
        let listed = if filter.since == "date" && filter.since_secs(chrono::Local::now()).is_none()
        {
            Err(anyhow::anyhow!("Pick the date to list imports since."))
        } else {
            listing(s, filter).await
        };
        match listed {
            Ok(rows) => Self {
                filter: filter.clone(),
                total: rows.len(),
                spares: rows.iter().filter(|r| r.spare().is_some()).count(),
                rows: Some(Rows::page(s, filter, rows, 0, held.max(SHOWN), false)),
                error: None,
            },
            Err(e) => Self {
                filter: filter.clone(),
                rows: None,
                total: 0,
                spares: 0,
                error: Some(format!("{e:#}")),
            },
        }
    }
}

/// A page of the list's rows.
#[derive(Template)]
#[template(path = "library/rows.html")]
struct Rows {
    root: String,
    filter: Filter,
    rows: Vec<Row>,
    /// Every album is selected, so the rows arrive ticked.
    all: bool,
    /// The button that fetches the page after.
    more: More,
}

/// The button that fetches the next page of the list, while there is one.
#[derive(Template)]
#[template(path = "library/more.html")]
struct More {
    /// Where the next page starts, and how many albums are past this one.
    next: Option<(usize, usize)>,
}

impl Rows {
    /// The page of `rows` starting at `from`.
    fn page(
        s: &UiState,
        filter: &Filter,
        rows: Vec<Row>,
        from: usize,
        take: usize,
        all: bool,
    ) -> Self {
        let total = rows.len();
        let rows: Vec<Row> = rows.into_iter().skip(from).take(take).collect();
        let end = from + rows.len();
        Self {
            root: s.app.library.root().to_string_lossy().into_owned(),
            filter: filter.clone(),
            rows,
            all,
            more: More {
                next: (end < total).then_some((end, total - end)),
            },
        }
    }
    fn rel<'a>(&self, p: &'a str) -> &'a str {
        rel(&self.root, p)
    }
    fn imported(&self, a: &LibraryAlbum) -> Option<kit::Time<'static>> {
        imported(a.added)
    }
    fn slot(&self, path: &str) -> String {
        slot_of(path)
    }
    fn title(&self, r: &Row) -> String {
        r.album
            .album
            .clone()
            .unwrap_or_else(|| self.rel(&r.album.path).to_string())
    }
    /// The address that opens or closes the album in its row.
    fn toggle(&self, path: &str) -> String {
        crate::web::place_url("/library/album", &[("path", path)])
    }
    /// The page with this album open, for a link opened in a new tab.
    fn href(&self, path: &str) -> String {
        let mut params = self.filter.params();
        params.push(("album", path));
        crate::web::place_url("/library", &params)
    }
}

/// The next page of the list, added below what it shows, and the button for
/// the page after.
pub(in crate::web) async fn more(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let filter = Filter::from_fields(&form);
    let from = field(&form, "offset")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let rows = match listing(&s, &filter).await {
        Ok(rows) => rows,
        Err(e) => return failed(&e),
    };
    let page = Rows::page(
        &s,
        &filter,
        rows,
        from,
        SHOWN,
        field(&form, "all").is_some(),
    );
    let held = from + page.rows.len();
    askama_datastar::events([
        insert("#lib-rows", ElementPatchMode::Append, &page.draw()),
        page.more.draw().patch().into(),
        // So the stream redraws as many rows as the page now holds.
        crate::web::signals(serde_json::json!({ "librows": held })),
    ])
}

/// The album at `path` opened in its row, or closed if it is open: the
/// address lists every open album, so a reload or a shared link opens them
/// again.
pub(in crate::web) async fn album(
    State(s): State<UiState>,
    Query(AlbumQuery { filter: _, path }): Query<AlbumQuery>,
    Signals(shown): Signals<Shown>,
) -> Response {
    let filter: Filter = shown.view();
    let mut open = shown.view::<Open>().albums();
    let slot = slot_of(&path);
    if open.contains(&path) {
        open.retain(|p| *p != path);
        return askama_datastar::events([
            askama_datastar::clear(&format!("#{slot}")).into(),
            place(&filter.url_with(&open)),
        ]);
    }
    open.push(path.clone());
    let url = filter.url_with(&open);
    askama_datastar::stream(async_stream::stream! {
        yield place(&url);
        yield album_html(&s, &filter, &path).await.patch().into();
        yield art_html(&s, &path).await.patch().into();
    })
    .into_response()
}

/// A region the server filled (a preview, an edit form) closed.
#[derive(serde::Deserialize)]
pub(in crate::web) struct DismissQuery {
    slot: String,
    part: String,
}

pub(in crate::web) async fn dismiss(Query(q): Query<DismissQuery>) -> Response {
    let part = match q.part.as_str() {
        "preview" | "edit" | "retag" => q.part.as_str(),
        _ => return http::StatusCode::BAD_REQUEST.into_response(),
    };
    let slot = slot_in(&[("slot".to_string(), q.slot)]);
    askama_datastar::events([askama_datastar::clear(&format!("#{slot}-{part}")).into()])
}

#[derive(Template)]
#[template(path = "library/album.html")]
struct AlbumView<'a> {
    root: &'a str,
    filter: &'a Filter,
    /// The element in its row it opens into.
    slot: String,
    row: &'a Row,
    tracks: Vec<sift::meta::Track>,
}

/// An album's art: the cover shown, the cover files beside it and the
/// pictures embedded in its files.
#[derive(Template)]
#[template(path = "library/art.html")]
struct ArtView {
    slot: String,
    title: String,
    art: Option<AlbumArt>,
    /// Where its cover is drawn from, when it has one.
    cover: Option<String>,
    error: Option<String>,
}

impl ArtView {
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
        format!("{}{size}, {}", c.name, crate::web::human(c.bytes))
    }
}

impl AlbumView<'_> {
    /// The address that closes it.
    fn toggle(&self) -> String {
        crate::web::place_url("/library/album", &[("path", self.row.album.path.as_str())])
    }
    fn bin_label(&self) -> &'static str {
        if self.row.spare().is_some() {
            "Bin this copy"
        } else {
            "Bin"
        }
    }
    fn image_input(&self) -> kit::Input<'static> {
        kit::Input::new(kit::InputKind::File, "image").attr("accept", "image/jpeg,image/png")
    }

    fn rel<'b>(&self, p: &'b str) -> &'b str {
        rel(self.root, p)
    }
    fn title(&self) -> String {
        title_of(&self.row.album)
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
            ("release type", list(&t.release_types)),
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

/// The album at `path`, open in its row: its files and their tags, where
/// the rules file it, and what can be done to it. Its art follows, from
/// [`art_html`].
pub(in crate::web) async fn album_html(s: &UiState, filter: &Filter, path: &str) -> Html {
    let root = s.app.library.root().to_string_lossy().into_owned();
    let slot = slot_of(path);
    let result = async {
        let library = &s.app.library;
        let album = LibraryAlbum::from(&library.album_at(path).await?);
        // The labels only: the last overview rather than waiting for the
        // next, which takes seconds and is worked out after every import.
        let marks = marks(&*library.overview_soon().await?).remove(&album.path);
        let tracks = library.tracks(path).await?;
        anyhow::Ok((Row { album, marks }, tracks))
    }
    .await;
    match result {
        Ok((row, tracks)) => AlbumView {
            root: &root,
            filter,
            slot,
            row: &row,
            tracks,
        }
        .draw(),
        Err(e) => AlbumTrouble {
            slot,
            error: format!("{e:#}"),
        }
        .draw(),
    }
}

/// What stands in an album's row when it cannot be read.
#[derive(Template)]
#[template(path = "library/album_trouble.html")]
struct AlbumTrouble {
    slot: String,
    error: String,
}

/// The art of the album at `path`, for its open detail.
pub(in crate::web) async fn art_html(s: &UiState, path: &str) -> Html {
    let slot = slot_of(path);
    match s.app.library.art(path).await {
        Ok(art) => ArtView {
            cover: cover_url(&art),
            title: std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            slot,
            art: Some(art),
            error: None,
        }
        .draw(),
        Err(e) => ArtView {
            cover: None,
            title: String::new(),
            slot,
            art: None,
            error: Some(format!("{e:#}")),
        }
        .draw(),
    }
}

/// The address of the album's cover, when it has one, versioned by the
/// file it comes from so a browser keeps it until the art changes. The
/// file is the one [`crate::library::Library::cover`] serves: the leading
/// cover image, or else the first file with a picture embedded.
fn cover_url(art: &AlbumArt) -> Option<String> {
    let source = match art.cover_files.first() {
        Some(c) => std::path::Path::new(&art.path).join(&c.name),
        None => {
            let mut with: Vec<&str> = art
                .files
                .iter()
                .filter(|f| !f.pictures.is_empty())
                .map(|f| f.file.as_str())
                .collect();
            with.sort();
            std::path::Path::new(&art.path).join(with.first()?)
        }
    };
    let meta = std::fs::metadata(&source).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let version = key(&format!("{}:{}:{modified}", source.display(), meta.len()));
    Some(crate::web::place_url(
        "/library/album/cover",
        &[("path", &art.path), ("v", &version)],
    ))
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct CoverQuery {
    path: String,
    #[serde(default)]
    v: String,
}

/// An album's cover: its leading cover image, or the front cover embedded
/// in its first file. Only albums the library's index holds are served. A
/// versioned address names one version of the art, so it is kept for good.
pub(in crate::web) async fn cover(
    State(s): State<UiState>,
    Query(q): Query<CoverQuery>,
) -> Response {
    let Ok(Some((bytes, _))) = s.app.library.cover(&q.path).await else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    image_response(bytes, !q.v.is_empty())
}

pub(in crate::web) fn image_response(bytes: Vec<u8>, immutable: bool) -> Response {
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
pub(in crate::web) type ArtStash = std::sync::Arc<
    parking_lot::Mutex<HashMap<String, (std::time::Instant, std::sync::Arc<Image>)>>,
>;

/// How long a previewed image waits to be applied, and how many wait.
const ART_KEPT: std::time::Duration = std::time::Duration::from_secs(3600);
pub(in crate::web) const ART_HELD: usize = 4;

pub(in crate::web) fn stash(art: &ArtStash, image: Image) -> String {
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

pub(in crate::web) fn stashed(art: &ArtStash, token: &str) -> Option<std::sync::Arc<Image>> {
    art.lock()
        .get(token)
        .filter(|(at, _)| at.elapsed() < ART_KEPT)
        .map(|(_, i)| i.clone())
}

/// An image previewed as an album's new art.
pub(in crate::web) async fn pending_art(
    State(s): State<UiState>,
    Path(token): Path<String>,
) -> Response {
    match stashed(&s.art, &token) {
        Some(image) => image_response(image.bytes.clone(), true),
        None => http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// New art for the album open in detail, from an uploaded image or a URL:
/// read and checked, kept for applying, and previewed as any other action.
pub(in crate::web) async fn art_preview(
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
    let url = field(&form, "url")
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
    parts.push(crate::web::human(p.bytes as u64));
    parts.join(", ")
}

#[derive(Template)]
#[template(path = "library/releases.html")]
struct ReleasesView<'a> {
    path: &'a str,
    slot: String,
    filter: &'a Filter,
    candidates: Vec<sift::Candidate>,
    review: Option<String>,
    error: Option<String>,
    discogs: bool,
    /// MusicBrainz is still being asked.
    asking: bool,
}

impl ReleasesView<'_> {
    fn paste_hint(&self) -> &'static str {
        if self.discogs {
            "Or paste a MusicBrainz or Discogs release id or link"
        } else {
            "Or paste a MusicBrainz release id or link"
        }
    }
}

/// The releases MusicBrainz (and Discogs, when enabled) has for an album's
/// files, closest first, to re-tag it from; or one pasted.
pub(in crate::web) async fn releases(
    State(s): State<UiState>,
    Query(AlbumQuery { filter, path }): Query<AlbumQuery>,
) -> Response {
    let slot = slot_of(&path);
    askama_datastar::stream(async_stream::stream! {
        let discogs = s.app.jobs.discogs();
        let mut view = ReleasesView { path: &path, slot, filter: &filter, candidates: Vec::new(), review: None, error: None, discogs, asking: true };
        yield Event::from(view.draw().patch());
        view.asking = false;
        match s.app.library.rematch(&path).await {
            Ok(r) => (view.candidates, view.review) = (r.candidates, r.review),
            Err(e) => view.error = Some(format!("{e:#}")),
        }
        yield Event::from(view.draw().patch());
    })
    .into_response()
}

/// The albums ticked in the list (or the one open in detail), or with
/// `all`, every album the filter admits, in list order.
async fn selection(s: &UiState, form: &[(String, String)]) -> anyhow::Result<(Filter, Vec<Row>)> {
    let filter = Filter::from_fields(form);
    if field(form, "all").is_some() {
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
