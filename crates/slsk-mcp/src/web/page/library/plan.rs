//! A library action previewed album by album before anything changes: what
//! each would do, worked out from the same `Library` calls applying makes,
//! with the tags typed into an edit form read back into edits.

use std::collections::HashMap;

use askama::Template;
use askama_datastar::ElementPatchMode;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use koan_kit_web as kit;

use super::{AlbumQuery, Dup, Filter, Row, key, rel, selection, slot_in, slot_of, stashed};
use crate::art::{ArtRemoved, ArtSet, Image, Target};
use crate::library::{CreditWrite, LibraryAlbum, Preview, TagEdits, by_path, release_id};
use crate::web::{Draw, UiState, failed, field, fields, insert};

/// What can be done to the albums ticked in the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::web) enum Action {
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
    pub(in crate::web) fn parse(s: &str) -> Option<Self> {
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

    pub(in crate::web) fn slug(self) -> &'static str {
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

    pub(in crate::web) fn name(self) -> &'static str {
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

    pub(in crate::web) fn doing(self) -> &'static str {
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

    pub(in crate::web) fn done(self) -> &'static str {
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
    pub(in crate::web) fn about(self) -> &'static str {
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
    pub(in crate::web) fn cap(self) -> Option<usize> {
        match self {
            Self::Refile | Self::Bin | Self::Edit | Self::SetArt => None,
            Self::Credit | Self::Enrich | Self::Reimport | Self::RemoveArt => Some(50),
        }
    }
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
pub(in crate::web) struct ArtChoice {
    pub(in crate::web) token: String,
    pub(in crate::web) embed: bool,
    pub(in crate::web) file: bool,
}

impl ArtChoice {
    /// The choice the art form makes: embedding unless only a cover file
    /// is ticked, as imports embed.
    pub(in crate::web) fn from_form(form: &[(String, String)]) -> Option<Self> {
        let ticked = |k| field(form, k).is_some_and(|v| !v.is_empty());
        Some(Self {
            token: field(form, "art")?.to_string(),
            embed: ticked("embed"),
            file: ticked("file"),
        })
    }

    pub(in crate::web) fn target(&self) -> Target {
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
        types: Vec<String>,
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

pub(in crate::web) struct Lyric {
    pub(in crate::web) file: String,
    pub(in crate::web) synced: bool,
    pub(in crate::web) lines: usize,
    pub(in crate::web) first: String,
}

impl Lyric {
    pub(in crate::web) fn of(l: &sift::PlannedLyrics) -> Self {
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
#[template(path = "library/plan.html")]
struct PlanView<'a> {
    action: Action,
    filter: &'a Filter,
    /// Where it is drawn: `lib`, or an album's slot.
    slot: &'a str,
    /// Every album the filter admits was selected.
    all: bool,
    selected: usize,
    shown: usize,
    /// The separator credits are split on, carried to applying.
    split: &'a str,
}

#[derive(Template)]
#[template(path = "library/planned.html")]
struct PlannedView<'a> {
    p: &'a Planned,
    root: &'a str,
    slot: &'a str,
}

impl PlannedView<'_> {
    /// A row with nothing to apply has no box, so its summary is indented to line up with those that do.
    fn place(&self) -> &'static str {
        if self.p.applicable {
            "min-w-0 flex-1"
        } else {
            "min-w-0 flex-1 pl-6"
        }
    }
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
        "albumtypes" => "release type",
        "mb_albumid" => "MusicBrainz release",
        "mb_releasegroupid" => "MusicBrainz release group",
        "mb_trackid" => "MusicBrainz recording",
        "mb_releasetrackid" => "MusicBrainz track",
        "mb_artistids" => "MusicBrainz artists",
        "mb_albumartistids" => "MusicBrainz album artists",
        other => other,
    }
}

pub(in crate::web) fn title_of(a: &LibraryAlbum) -> String {
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
                    types: plan.release_types.clone(),
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
                    _ if plan.comparison.release.tracks == 0 => {
                        Some("This release has no tracklist on its site, so there is nothing to tag these files from. Add its tracks there, or pick another release.".into())
                    }
                    _ if plan.files.is_empty() => Some("None of these files match a track of this release.".into()),
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
pub(in crate::web) async fn preview(
    State(s): State<UiState>,
    Path(action): Path<String>,
    body: Bytes,
) -> Response {
    let Some(action) = Action::parse(&action) else {
        return failed(&anyhow::anyhow!("no such action"));
    };
    previewed(s, action, fields(&body)).await
}

pub(in crate::web) async fn previewed(
    s: UiState,
    action: Action,
    form: Vec<(String, String)>,
) -> Response {
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
    let slot = slot_in(&form);
    let all = field(&form, "all").is_some();
    askama_datastar::stream(async_stream::stream! {
        let selected = rows.len();
        let shown = action.cap().map_or(selected, |c| selected.min(c));
        let split = field(&form, "split").unwrap_or_default().trim().to_string();
        let shell = PlanView { action, filter: &filter, slot: &slot, all, selected, shown, split: &split };
        yield shell.draw().patch().into();
        let known = if matches!(action, Action::Credit | Action::Enrich) {
            match s.app.library.known_artists().await {
                Ok(k) => k.splitting_on(&split),
                Err(e) => {
                    yield kit::Flash::bad(format!("{e:#}")).patch().into();
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
            yield Progress {
                slot: &slot,
                working: true,
                words: format!("Working out {} of {shown}: {}", i + 1, title_of(&row.album)),
            }
            .draw()
            .patch()
            .into();
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
            let html = PlannedView { p: &p, root: &root, slot: &slot }.draw();
            yield insert(&format!("#{slot}-pv-albums"), ElementPatchMode::Append, &html);
        }
        yield Progress {
            slot: &slot,
            working: false,
            words: format!("{applicable} of {shown} would change."),
        }
        .draw()
        .patch()
        .into();
        yield ApplyRow { slot: &slot, action: (applicable > 0).then_some(action) }.draw().patch().into();
    })
    .into_response()
}

/// How far a preview has got, and then what it found.
#[derive(Template)]
#[template(path = "library/progress.html")]
struct Progress<'a> {
    slot: &'a str,
    working: bool,
    words: String,
}

/// A preview's last row: its apply, when anything applies, with the count of
/// what is ticked beside it, and cancel.
#[derive(Template)]
#[template(path = "library/apply.html")]
struct ApplyRow<'a> {
    slot: &'a str,
    action: Option<Action>,
}

impl ApplyRow<'_> {
    fn dismiss(&self) -> String {
        crate::web::place_url(
            "/library/dismiss",
            &[("slot", self.slot), ("part", "preview")],
        )
    }
}

/// Album-wide fields the edit form offers, by beets name, with their labels.
const ALBUM_FIELDS: &[(&str, &str)] = &[
    ("album", "album"),
    ("albumartist", "album artist"),
    ("date", "date"),
    ("original_date", "original date"),
    ("genre", "genre"),
    ("label", "label"),
    ("catalognum", "catalogue number"),
    ("albumtypes", "release type"),
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

/// MusicBrainz's primary release group types, as Picard writes them. A
/// release group has one at most.
const PRIMARY_TYPES: &[&str] = &["album", "single", "ep", "broadcast", "other"];

/// MusicBrainz's secondary release group types, as Picard writes them. Any
/// number of them go with a primary type.
const SECONDARY_TYPES: &[&str] = &[
    "compilation",
    "soundtrack",
    "spokenword",
    "interview",
    "audiobook",
    "audio drama",
    "live",
    "remix",
    "dj-mix",
    "mixtape/street",
    "demo",
    "field recording",
];

/// A value as sift will write it, so one that differs only in spelling or
/// order from what the files hold is not taken for a change. Release types
/// are lower case, "; " between them, the primary type first and then the
/// secondary types in MusicBrainz's order; a type MusicBrainz lacks keeps
/// its place after them.
fn typed_as_written(field: &str, typed: String) -> String {
    if field != "albumtypes" {
        return typed;
    }
    let mut types: Vec<String> = Vec::new();
    for t in typed.split(';').map(|t| t.trim().to_lowercase()) {
        if !t.is_empty() && !types.contains(&t) {
            types.push(t);
        }
    }
    let rank = |t: &String| {
        PRIMARY_TYPES
            .iter()
            .chain(SECONDARY_TYPES)
            .position(|k| k == t)
            .unwrap_or(usize::MAX)
    };
    types.sort_by_key(rank);
    types.join("; ")
}

/// A release type's primary type, when it starts with one.
fn primary_type(types: Option<String>) -> Option<String> {
    let types = typed_as_written("albumtypes", types?);
    let first = types.split("; ").next()?;
    PRIMARY_TYPES.contains(&first).then(|| first.to_string())
}

/// The primary type an album's files have, for secondary types chosen with
/// the primary type kept; a file with none takes the others'. Refused when
/// two files have different ones, since one value is written to them all.
fn kept_primary(tracks: &[sift::meta::Track]) -> anyhow::Result<Option<String>> {
    let mut primaries = tracks
        .iter()
        .filter_map(|t| primary_type(t.field("albumtypes")));
    let first = primaries.next();
    if primaries.any(|p| Some(&p) != first.as_ref()) {
        let album = tracks
            .iter()
            .find_map(|t| t.album.clone())
            .unwrap_or_default();
        anyhow::bail!(
            "{album}: its files have different primary release types, so choose one to go with the secondary types"
        );
    }
    Ok(first)
}

/// The release type the form's type controls ask for, as sift writes it, or
/// `None` for no change: a form without them, or one keeping the type as it
/// is. Secondary types chosen with the primary type kept go with the one
/// `tracks` share.
fn typed_release_type(
    form: &[(String, String)],
    tracks: &[sift::meta::Track],
) -> anyhow::Result<Option<String>> {
    let Some(primary) = field(form, "type-primary") else {
        return Ok(None);
    };
    let secondary = form
        .iter()
        .filter(|(k, _)| k == "type-secondary")
        .map(|(_, v)| v.as_str());
    let primary = match primary {
        "keep" if !form.iter().any(|(k, _)| k == "type-secondary") => return Ok(None),
        "keep" => kept_primary(tracks)?.unwrap_or_default(),
        "none" => String::new(),
        p => p.to_string(),
    };
    let typed = std::iter::once(primary.as_str())
        .chain(secondary)
        .collect::<Vec<_>>()
        .join(";");
    Ok(Some(typed_as_written("albumtypes", typed)))
}

/// The secondary types to offer as toggles, each with whether `current`
/// has it: MusicBrainz's, then any other type the files hold beside the
/// primary type (a second primary type, or one MusicBrainz lacks), so it is
/// kept unless taken off.
fn type_choices(current: Option<String>) -> Vec<(String, bool)> {
    let current: Vec<String> = typed_as_written("albumtypes", current.unwrap_or_default())
        .split("; ")
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let has = |t: &str| current.iter().any(|c| c == t);
    let primary = current
        .first()
        .filter(|c| PRIMARY_TYPES.contains(&c.as_str()));
    SECONDARY_TYPES
        .iter()
        .map(|t| (t.to_string(), has(t)))
        .chain(
            current
                .iter()
                .filter(|c| Some(*c) != primary && !SECONDARY_TYPES.contains(&c.as_str()))
                .map(|c| (c.clone(), true)),
        )
        .collect()
}

/// What every file has for `field`, as it would be written, and whether
/// they disagree.
fn consensus(tracks: &[sift::meta::Track], field: &str) -> (Option<String>, bool) {
    let mut values = tracks
        .iter()
        .map(|t| t.field(field).map(|v| typed_as_written(field, v)));
    let first = values.next().flatten();
    let varies = values.any(|v| v != first);
    (if varies { None } else { first }, varies)
}

/// The edits the form asks for against the files as they are now: only
/// fields typed differently from what the files have. An album-wide field
/// the files disagree on and left blank is left alone; blanking one they
/// agree on clears it. A number typed as anything but digits is refused,
/// rather than written as no number at all.
pub(in crate::web) fn edits_from(
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
        let typed = match *field {
            "albumtypes" => typed_release_type(form, tracks)?,
            _ => raw(&format!("a-{field}")),
        };
        let Some(typed) = typed else {
            continue;
        };
        let (now, varies) = consensus(tracks, field);
        let now = typed_as_written(field, now.unwrap_or_default().trim().to_string());
        // A release type is chosen rather than typed, so none chosen is a choice.
        if (varies && typed.is_empty() && *field != "albumtypes") || (!varies && typed == now) {
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
pub(in crate::web) fn batch_edits_from(
    tracks: &[sift::meta::Track],
    form: &[(String, String)],
) -> anyhow::Result<TagEdits> {
    let mut edits = TagEdits::default();
    let mut asked = false;
    for (field, label) in batch_fields() {
        let clear = crate::web::field(form, &format!("clear-{field}")).is_some();
        let typed = match field {
            // Cleared, the type's other controls are not read.
            "albumtypes" if clear => None,
            "albumtypes" => typed_release_type(form, tracks)?,
            _ => crate::web::field(form, &format!("a-{field}")).map(|v| v.trim().to_string()),
        }
        .unwrap_or_default();
        let typed = typed.as_str();
        let value = match (typed.is_empty(), clear) {
            (true, false) => continue,
            (false, true) => anyhow::bail!("{label}: type a value or tick Clear, not both"),
            (true, true) => None,
            (false, false) => Some(typed.to_string()),
        };
        asked = true;
        if tracks.iter().all(|t| {
            t.field(field)
                .map(|v| typed_as_written(field, v.trim().to_string()))
                == value
        }) {
            continue;
        }
        edits.album.push((field.to_string(), value));
    }
    // An artist renamed wherever a file's whole artist or album artist is
    // that name, in any case; a collaboration naming it is left alone. The
    // name is replaced in the file's list of credits too, so its MusicBrainz
    // ids stay with it.
    let from = field(form, "rename-from").map_or("", str::trim);
    let to = field(form, "rename-to").map_or("", str::trim);
    if from.is_empty() != to.is_empty() {
        anyhow::bail!("Rename an artist: type the name as the files have it and the new name.");
    }
    if !from.is_empty() {
        if field(form, "a-albumartist").is_some_and(|v| !v.trim().is_empty())
            || field(form, "clear-albumartist").is_some()
        {
            anyhow::bail!("Set the album artist or rename an artist, not both.");
        }
        asked = true;
        let named = |v: &str| v.trim().to_lowercase() == from.to_lowercase();
        for t in tracks {
            let mut changes = Vec::new();
            for (field, list, credits) in [
                ("artist", "artists", &t.artists),
                ("albumartist", "albumartists", &t.album_artists),
            ] {
                // A file already as asked is left alone.
                if !t.field(field).is_some_and(|v| named(&v) && v.trim() != to) {
                    continue;
                }
                changes.push((field.to_string(), Some(to.to_string())));
                if credits.iter().any(|c| named(c)) {
                    let renamed: Vec<&str> = credits
                        .iter()
                        .map(|c| if named(c) { to } else { c.as_str() })
                        .collect();
                    changes.push((list.to_string(), Some(renamed.join("; "))));
                }
            }
            if !changes.is_empty() {
                edits.tracks.push((t.path.clone(), changes));
            }
        }
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
            let release = match field(form, "pasted") {
                Some(p) => Some(release_id(p).ok_or_else(|| {
                    anyhow::anyhow!("{p:?} is not a MusicBrainz or Discogs release id or link")
                })?),
                None => field(form, "release").map(str::to_string),
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
        Action::Edit if field(form, "batch").is_some() => {
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

#[derive(Template)]
#[template(path = "library/edit.html")]
struct EditView<'a> {
    path: &'a str,
    slot: String,
    filter: &'a Filter,
    tracks: Vec<sift::meta::Track>,
}

impl EditView<'_> {
    fn differs(&self, field: &str) -> &'static str {
        if self.varies(field) {
            "(differs between files)"
        } else {
            ""
        }
    }

    fn fields(&self) -> &'static [(&'static str, &'static str)] {
        ALBUM_FIELDS
    }
    fn now(&self, field: &str) -> String {
        consensus(&self.tracks, field).0.unwrap_or_default()
    }
    fn varies(&self, field: &str) -> bool {
        consensus(&self.tracks, field).1
    }
    fn primary(&self) -> Option<String> {
        primary_type(consensus(&self.tracks, "albumtypes").0)
    }
    fn is_primary(&self, t: &str) -> bool {
        PRIMARY_TYPES.contains(&t)
    }
    fn type_choices(&self) -> Vec<(String, bool)> {
        type_choices(consensus(&self.tracks, "albumtypes").0)
    }
    /// The primary type as the files have it, or `keep` when they differ.
    fn primary_select(&self) -> kit::Select<'static> {
        let varies = self.varies("albumtypes");
        let select = kit::Select::new("type-primary")
            .label("Primary release type")
            .id(format!("{}-type", self.slot))
            .datastar(
                "on:change",
                "el.closest('.field').querySelectorAll('[data-primary] input').forEach(c => c.checked = false)",
            );
        let select = if varies {
            select.option("keep", "(differs between files)")
        } else {
            select
        };
        let chosen = if varies {
            "keep".to_string()
        } else {
            self.primary().unwrap_or_else(|| "none".to_string())
        };
        select
            .option("none", "none")
            .options(PRIMARY_TYPES.iter().map(|p| (*p, *p)))
            .selected(chosen)
    }
    fn value(&self, t: &sift::meta::Track, field: &str) -> String {
        t.field(field).unwrap_or_default()
    }
    fn file(&self, t: &sift::meta::Track) -> String {
        t.path.to_string_lossy().into_owned()
    }
    fn dismiss(&self) -> String {
        crate::web::place_url(
            "/library/dismiss",
            &[("slot", self.slot.as_str()), ("part", "edit")],
        )
    }
}

/// The album's tags as a form: album-wide fields, then each file's own.
pub(in crate::web) async fn edit_form(
    State(s): State<UiState>,
    Query(AlbumQuery { filter, path }): Query<AlbumQuery>,
) -> Response {
    match s.app.library.tracks(&path).await {
        Ok(tracks) => crate::web::one(
            EditView {
                slot: slot_of(&path),
                path: &path,
                filter: &filter,
                tracks,
            }
            .draw(),
        ),
        Err(e) => failed(&e),
    }
}

#[derive(Template)]
#[template(path = "library/batch.html")]
struct BatchView<'a> {
    filter: &'a Filter,
    rows: &'a [Row],
}

impl BatchView<'_> {
    fn fields(&self) -> Vec<(&'static str, &'static str)> {
        batch_fields().collect()
    }
    fn is_primary(&self, t: &str) -> bool {
        PRIMARY_TYPES.contains(&t)
    }
    fn type_choices(&self) -> Vec<(String, bool)> {
        type_choices(None)
    }
    fn primary_select(&self) -> kit::Select<'static> {
        kit::Select::new("type-primary")
            .label("Primary release type")
            .option("keep", "leave as is")
            .options(PRIMARY_TYPES.iter().map(|p| (*p, *p)))
            .selected("keep")
    }
}

/// Album-wide fields for the albums ticked in the list, set on all of them.
pub(in crate::web) async fn batch_form(State(s): State<UiState>, body: Bytes) -> Response {
    let (filter, rows) = match selection(&s, &fields(&body)).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    if rows.is_empty() {
        return failed(&anyhow::anyhow!("nothing selected"));
    }
    crate::web::one(
        BatchView {
            filter: &filter,
            rows: &rows,
        }
        .draw(),
    )
}
