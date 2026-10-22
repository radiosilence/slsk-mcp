//! The library as a whole, through sift's index: listing it, re-filing it
//! under the current path rules, and moving spare copies of albums to a bin
//! beside it.
//!
//! The bin is outside the library directory, so neither Navidrome nor the
//! shares see what is in it, and on the same drive, so binning is a rename.
//! Nothing here deletes a file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_graphql::SimpleObject;
use sift::library::{Album, Query, Value};
use sift::manage::{self, Plan};
use tokio::sync::Mutex;

pub struct Library {
    /// Answers queries, from the index as last refreshed. Its own
    /// connection, so a read is never queued behind a scan: under SQLite's
    /// WAL it sees the last committed state while the writer works.
    reader: Mutex<sift::library::Library>,
    /// Scans the files, and makes every change to them.
    writer: Mutex<sift::library::Library>,
    /// Wakes the refresh loop.
    stale: tokio::sync::Notify,
    /// Bumped whenever the index changes: by a refresh that found changes,
    /// or a change made through the writer.
    generation: AtomicU64,
    /// The whole library's duplicates and re-file plan, and the generation
    /// they were worked out at. Planning every track's destination takes
    /// seconds on a large library, and the answer changes only when the
    /// library does.
    overview: Mutex<Option<(u64, Arc<Overview>)>>,
    cfg: sift::Config,
    bin: PathBuf,
    tagger: std::sync::Arc<sift::Importer>,
}

#[derive(SimpleObject, Clone)]
pub struct LibraryAlbum {
    pub path: String,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    pub format: String,
    pub tracks: usize,
    /// When the album's first file was indexed, in seconds since the epoch.
    pub added: Option<i64>,
}

impl From<&Album> for LibraryAlbum {
    fn from(a: &Album) -> Self {
        let text = |f: &str| {
            a.field(f).map(|v| match v {
                Value::Text(s) => s,
                Value::Number(n) => format!("{n:.0}"),
            })
        };
        Self {
            path: a.dir.to_string_lossy().into_owned(),
            album_artist: text("albumartist"),
            album: text("album"),
            year: text("year"),
            format: text("format").unwrap_or_default(),
            tracks: a.items.len(),
            added: a.items.iter().map(|i| i.added).min(),
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct SpareCopy {
    pub album: LibraryAlbum,
    /// Why this copy is the spare: "lossy copy of a lossless album", …
    pub reason: String,
    /// Where it went, once binned.
    pub binned_to: Option<String>,
}

/// Hand edits to one album's tags: fields set on every file, and fields set
/// on single files. See [`Library::edit_plan`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TagEdits {
    pub album: Vec<sift::meta::Change>,
    pub tracks: Vec<(PathBuf, Vec<sift::meta::Change>)>,
}

impl TagEdits {
    pub fn is_empty(&self) -> bool {
        self.album.is_empty() && self.tracks.iter().all(|(_, c)| c.is_empty())
    }

    /// Every change to the file at `path`: the album's, then its own.
    pub fn for_file(&self, path: &Path) -> Vec<sift::meta::Change> {
        let mut out = self.album.clone();
        for (p, c) in &self.tracks {
            if p == path {
                out.extend(c.iter().cloned());
            }
        }
        out
    }

    /// A credit typed by hand no longer matches the file's list of artists
    /// and their ids, so those are cleared; Credit artists splits it again.
    pub fn clear_stale_credits(&mut self, tracks: &[sift::meta::Track]) {
        for t in tracks {
            let all = self.for_file(&t.path);
            let edits_field = |f: &str| all.iter().any(|(n, _)| n == f);
            let mut clear = Vec::new();
            if edits_field("artist") && !t.artists.is_empty() && !edits_field("artists") {
                clear.push(("artists".to_string(), None));
            }
            if edits_field("albumartist")
                && !t.album_artists.is_empty()
                && !edits_field("albumartists")
            {
                clear.push(("albumartists".to_string(), None));
            }
            if clear.is_empty() {
                continue;
            }
            match self.tracks.iter_mut().find(|(p, _)| *p == t.path) {
                Some((_, c)) => c.extend(clear),
                None => self.tracks.push((t.path.clone(), clear)),
            }
        }
    }
}

/// What a hand edit would change. See [`Library::edit_plan`].
pub struct EditPlan {
    /// Each file and the fields that would change in it.
    pub files: Vec<(PathBuf, Vec<sift::meta::TagChange>)>,
    /// Where the edited tags would file the album.
    pub refile: Preview,
}

/// See [`Library::preview`].
pub enum Preview {
    InPlace,
    Refused(String),
    /// Every file, from and to: the tracks, and the rest of the folder when
    /// the whole album moves together.
    Moves(Vec<(PathBuf, PathBuf)>),
}

/// See [`Library::overview`].
pub struct Overview {
    pub duplicates: Vec<DuplicateSet>,
    pub plan: Vec<AlbumMove>,
}

#[derive(SimpleObject, Clone)]
pub struct DuplicateSet {
    pub keep: LibraryAlbum,
    pub spares: Vec<SpareCopy>,
}

#[derive(SimpleObject)]
pub struct EnrichedAlbum {
    pub path: String,
    pub gain_db: Option<f64>,
    pub genres: Vec<String>,
    /// Tracks given lyrics.
    pub lyrics: usize,
    /// With `preview`, the files that would be given lyrics.
    pub lyric_files: Vec<String>,
    /// Artists credited one by one where the files named them only together.
    pub artists: Vec<String>,
    pub problems: Vec<String>,
}

impl EnrichedAlbum {
    /// What a preview reports: the plan's additions, gain unmeasured.
    pub fn planned(path: String, p: &sift::EnrichPlan) -> Self {
        let mut artists: Vec<String> = Vec::new();
        for c in &p.credits.files {
            for a in c.artists.iter().chain(&c.album_artists) {
                if !artists.contains(a) {
                    artists.push(a.clone());
                }
            }
        }
        Self {
            path,
            gain_db: None,
            genres: p.genres.clone(),
            lyrics: p.lyrics.len(),
            lyric_files: p.lyrics.iter().map(|l| file_name(&l.path)).collect(),
            artists,
            problems: p.problems.clone(),
        }
    }
}

#[derive(SimpleObject)]
pub struct CreditedAlbum {
    pub path: String,
    /// Files given artist credits, or with `preview`, that would be.
    pub files: usize,
    pub artists: Vec<String>,
    /// With `preview`, each file's credits as they would be written.
    pub planned: Vec<CreditWrite>,
    pub problems: Vec<String>,
}

/// The credits one file would be given. A list left empty is not written.
#[derive(SimpleObject, Clone)]
pub struct CreditWrite {
    /// The file's name within its album.
    pub file: String,
    pub artists: Vec<String>,
    pub artist_ids: Vec<String>,
    pub album_artists: Vec<String>,
    pub album_artist_ids: Vec<String>,
    /// The MusicBrainz release the credits come from; absent when the
    /// file's own credit was split by artists the library holds.
    pub release: Option<String>,
}

impl From<&sift::PlannedCredit> for CreditWrite {
    fn from(c: &sift::PlannedCredit) -> Self {
        Self {
            file: file_name(&c.path),
            artists: c.artists.clone(),
            artist_ids: c.artist_ids.clone(),
            album_artists: c.album_artists.clone(),
            album_artist_ids: c.album_artist_ids.clone(),
            release: c.release.clone(),
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// What re-importing an album did, or with `preview`, would do.
#[derive(SimpleObject, Clone)]
pub struct ReimportedAlbum {
    pub path: String,
    /// The release applied, or with `preview`, the closest found.
    pub release: Option<ReleaseMatch>,
    /// Why a re-import without a release id stopped, or with `preview`,
    /// would stop, for a choice.
    pub review: Option<String>,
    /// Where the album is filed afterwards.
    pub to: Option<String>,
    pub problems: Vec<String>,
}

#[derive(SimpleObject, Clone)]
pub struct ReleaseMatch {
    pub id: String,
    pub artist: String,
    pub title: String,
    pub date: Option<String>,
    pub distance: f64,
    pub missing: usize,
    pub extra: usize,
}

impl From<&sift::Candidate> for ReleaseMatch {
    fn from(c: &sift::Candidate) -> Self {
        Self {
            id: c.id.clone(),
            artist: c.artist.clone(),
            title: c.title.clone(),
            date: c.date.clone(),
            distance: c.distance,
            missing: c.missing,
            extra: c.extra,
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct AlbumMove {
    pub from: String,
    /// The album's new directory; absent when it was refused.
    pub to: Option<String>,
    /// Why the album was left where it is.
    pub refused: Option<String>,
    pub files: usize,
}

/// One album's edits for [`Library::edit_many`].
#[derive(Debug)]
pub struct AlbumEdit {
    pub dir: String,
    pub edits: TagEdits,
    /// The files as the edits were worked out from, when they were: a file
    /// that reads differently by the time it is written is refused rather
    /// than written over.
    pub read: Option<Vec<sift::meta::Track>>,
}

/// Every album's edits name only editable fields, and no album is given
/// twice.
pub fn check_batch(albums: &[AlbumEdit]) -> anyhow::Result<()> {
    for (i, a) in albums.iter().enumerate() {
        check_editable(&a.edits)?;
        if albums[..i].iter().any(|b| b.dir == a.dir) {
            anyhow::bail!("{} is given more than once", a.dir);
        }
    }
    Ok(())
}

/// Terms that match exactly the albums at `paths`, as alternatives.
pub fn by_path<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut query = Vec::new();
    for p in paths {
        if !query.is_empty() {
            query.push(",".to_string());
        }
        query.push(format!("path:={p}"));
    }
    query
}

/// Edits only name fields `meta::set` writes.
fn check_editable(edits: &TagEdits) -> anyhow::Result<()> {
    let all = edits
        .album
        .iter()
        .chain(edits.tracks.iter().flat_map(|(_, c)| c));
    for (field, _) in all {
        if !sift::meta::EDITABLE.contains(&field.as_str()) {
            anyhow::bail!("{field} is not a field that can be edited");
        }
    }
    Ok(())
}

impl Library {
    pub fn open(
        index: &Path,
        tagger: std::sync::Arc<sift::Importer>,
        bin: PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            writer: Mutex::new(sift::library::Library::open(index)?.with_workers(2)),
            reader: Mutex::new(sift::library::Library::open(index)?),
            stale: tokio::sync::Notify::new(),
            generation: AtomicU64::new(0),
            overview: Mutex::new(None),
            cfg: tagger.cfg.clone(),
            bin,
            tagger,
        })
    }

    pub fn root(&self) -> &Path {
        &self.cfg.directory
    }

    /// Where spare and replaced copies go.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Bring the index up to date with the files. Incremental: only files
    /// whose size or modification time changed are read.
    fn refresh(&self, index: &mut sift::library::Library) -> anyhow::Result<()> {
        let report = tokio::task::block_in_place(|| index.update(&self.cfg.directory))?;
        if report.added + report.changed + report.removed > 0 {
            self.bump();
            tracing::info!(
                added = report.added,
                changed = report.changed,
                removed = report.removed,
                unreadable = report.failed.len(),
                "library index updated"
            );
        }
        Ok(())
    }

    /// Keep the index following the files: at startup, whenever
    /// [`Self::changed`] is called, and every so often for changes made
    /// outside this service. Requests that arrive during a refresh coalesce
    /// into one more.
    pub async fn follow(&self) {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        loop {
            {
                let mut index = self.writer.lock().await;
                if let Err(e) = self.refresh(&mut index) {
                    tracing::warn!(error = %e, "library index update failed");
                }
            }
            // Worked out now, so the next look at the library does not wait.
            if let Err(e) = self.overview().await {
                tracing::warn!(error = %e, "library overview failed");
            }
            tokio::select! {
                () = self.stale.notified() => {}
                () = tokio::time::sleep(EVERY) => {}
            }
        }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The whole library's duplicates and re-file plan, worked out once per
    /// change to the index. Concurrent callers wait for one computation.
    pub async fn overview(&self) -> anyhow::Result<Arc<Overview>> {
        let mut cached = self.overview.lock().await;
        // Read before the albums: a change landing during the computation
        // leaves this generation behind, and the next call works it out again.
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some((at, overview)) = cached.as_ref()
            && *at == generation
        {
            return Ok(overview.clone());
        }
        let albums = self.read(&[]).await?;
        let overview = Arc::new(tokio::task::block_in_place(|| Overview {
            duplicates: self.duplicates_of(&albums),
            plan: self.plan_of(&albums),
        }));
        *cached = Some((generation, overview.clone()));
        Ok(overview)
    }

    fn duplicates_of(&self, albums: &[Album]) -> Vec<DuplicateSet> {
        manage::duplicates(&self.cfg, albums)
            .into_iter()
            .map(|d| DuplicateSet {
                keep: LibraryAlbum::from(d.keep),
                spares: d
                    .others
                    .iter()
                    .map(|(album, reason)| SpareCopy {
                        album: LibraryAlbum::from(*album),
                        reason: reason.to_string(),
                        binned_to: None,
                    })
                    .collect(),
            })
            .collect()
    }

    fn plan_of(&self, albums: &[Album]) -> Vec<AlbumMove> {
        albums
            .iter()
            .filter_map(|album| {
                let from = album.dir.to_string_lossy().into_owned();
                match manage::plan_move(&self.cfg, album) {
                    Plan::InPlace => None,
                    Plan::Refused(why) => Some(AlbumMove {
                        from,
                        to: None,
                        refused: Some(why),
                        files: 0,
                    }),
                    Plan::Moves(moves) => Some(AlbumMove {
                        from,
                        to: moves
                            .first()
                            .and_then(|m| m.to.parent())
                            .map(|p| p.to_string_lossy().into_owned()),
                        refused: None,
                        files: moves.len(),
                    }),
                }
            })
            .collect()
    }

    /// The files changed under the index: have it read them again soon.
    pub fn changed(&self) {
        self.stale.notify_one();
    }

    /// The writer, after bringing the index up to date: a change is made
    /// only to what is there now.
    async fn write(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, sift::library::Library>> {
        let mut index = self.writer.lock().await;
        self.refresh(&mut index)?;
        Ok(index)
    }

    /// Albums matching `query`, from the index as it stands.
    async fn read(&self, query: &[String]) -> anyhow::Result<Vec<Album>> {
        let query = Query::parse(query)?;
        let index = self.reader.lock().await;
        Ok(tokio::task::block_in_place(|| index.albums(&query))?)
    }

    pub async fn albums(&self, query: &[String]) -> anyhow::Result<Vec<LibraryAlbum>> {
        Ok(self
            .read(query)
            .await?
            .iter()
            .map(LibraryAlbum::from)
            .collect())
    }

    /// Albums held more than once. With `bin`, every spare copy is moved to
    /// the bin.
    pub async fn duplicates(
        &self,
        query: &[String],
        bin: bool,
    ) -> anyhow::Result<Vec<DuplicateSet>> {
        if !bin {
            if query.is_empty() {
                return Ok(self.overview().await?.duplicates.clone());
            }
            let albums = self.read(query).await?;
            return Ok(tokio::task::block_in_place(|| self.duplicates_of(&albums)));
        }
        let mut index = self.write().await?;
        let albums = tokio::task::block_in_place(|| index.albums(&Query::parse(query)?))?;
        let mut out = Vec::new();
        for d in manage::duplicates(&self.cfg, &albums) {
            let mut spares = Vec::new();
            for (album, reason) in &d.others {
                // One spare that cannot move (the bin holds that path) is
                // reported and the rest still go.
                let to = match manage::bin(&mut index, &self.cfg.directory, &self.bin, album).await
                {
                    Ok(to) => to,
                    Err(e) => {
                        spares.push(SpareCopy {
                            album: LibraryAlbum::from(*album),
                            reason: format!("{reason}; not binned: {e}"),
                            binned_to: None,
                        });
                        continue;
                    }
                };
                self.bump();
                tracing::info!(
                    from = %album.dir.display(),
                    to = %to.display(),
                    reason,
                    "binned a spare copy"
                );
                spares.push(SpareCopy {
                    album: LibraryAlbum::from(*album),
                    reason: reason.to_string(),
                    binned_to: Some(to.to_string_lossy().into_owned()),
                });
            }
            out.push(DuplicateSet {
                keep: LibraryAlbum::from(d.keep),
                spares,
            });
        }
        Ok(out)
    }

    /// Where binning the album at `dir` would put it, or why it cannot be
    /// binned: the bin already holds that path, or it is not in the library.
    pub fn bin_destination(&self, dir: &str) -> Result<PathBuf, String> {
        let rel = Path::new(dir)
            .strip_prefix(&self.cfg.directory)
            .map_err(|_| "not under the library".to_string())?;
        let dest = self.bin.join(rel);
        if dest.exists() {
            return Err(format!("the bin already holds {}", dest.display()));
        }
        Ok(dest)
    }

    /// Move the album at `dir` to the bin, whether or not another copy is
    /// held. Nothing is deleted.
    pub async fn bin_album(&self, dir: &str) -> anyhow::Result<PathBuf> {
        let mut index = self.write().await?;
        let albums = tokio::task::block_in_place(|| index.albums(&Query::parse(&by_path([dir]))?))?;
        let album = albums
            .iter()
            .find(|a| a.dir == Path::new(dir))
            .ok_or_else(|| anyhow::anyhow!("no longer in the library"))?;
        let to = manage::bin(&mut index, &self.cfg.directory, &self.bin, album).await?;
        self.bump();
        tracing::info!(from = dir, to = %to.display(), "binned an album");
        Ok(to)
    }

    /// Every artist the library holds an album by: what a collaboration's
    /// credit is split against.
    pub async fn known_artists(&self) -> anyhow::Result<sift::credits::Known> {
        let albums = self.read(&[]).await?;
        Ok(sift::credits::Known::from_albums(&albums))
    }

    /// Add gain, genres, lyrics and each artist's credit to matching
    /// albums, as every new import gets. Rewrites tags (only those fields)
    /// across what the query matches, so it is for a deliberate backfill.
    pub async fn enrich(&self, query: &[String]) -> anyhow::Result<Vec<EnrichedAlbum>> {
        let known = self.known_artists().await?;
        self.enrich_with(query, &known).await
    }

    /// [`Self::enrich`] against artists already listed, for a caller
    /// enriching album by album that should not read the index each time.
    pub async fn enrich_with(
        &self,
        query: &[String],
        known: &sift::credits::Known,
    ) -> anyhow::Result<Vec<EnrichedAlbum>> {
        let mut out = Vec::new();
        for a in self.read(query).await? {
            let e = self.tagger.enrich(&a.dir, known).await?;
            out.push(EnrichedAlbum {
                path: a.dir.to_string_lossy().into_owned(),
                gain_db: e.gain_db,
                genres: e.genres,
                lyrics: e.lyrics,
                lyric_files: Vec::new(),
                artists: e.artists,
                problems: e.problems,
            });
        }
        self.changed();
        Ok(out)
    }

    /// Credit each artist on matching albums whose files name them only in
    /// one string: from their MusicBrainz release, or by splitting the
    /// credit where every part is an artist the library holds. Files that
    /// already carry the credits are left alone, so it can run again.
    pub async fn credit(&self, query: &[String]) -> anyhow::Result<Vec<CreditedAlbum>> {
        let known = self.known_artists().await?;
        self.credit_with(query, &known).await
    }

    /// [`Self::credit`] against artists already listed.
    pub async fn credit_with(
        &self,
        query: &[String],
        known: &sift::credits::Known,
    ) -> anyhow::Result<Vec<CreditedAlbum>> {
        let mut out = Vec::new();
        for a in self.read(query).await? {
            // One unreadable album (moved since the index last looked, a
            // corrupt file) is reported and passed over, so a long run
            // still finishes.
            let c = match self.tagger.credit(&a.dir, known).await {
                Ok(c) => c,
                Err(e) => sift::Credited {
                    problems: vec![format!("{e:#}")],
                    ..Default::default()
                },
            };
            if c.files == 0 && c.problems.is_empty() {
                continue;
            }
            out.push(CreditedAlbum {
                path: a.dir.to_string_lossy().into_owned(),
                files: c.files,
                artists: c.artists,
                planned: Vec::new(),
                problems: c.problems,
            });
        }
        self.changed();
        Ok(out)
    }

    /// What [`Self::credit`] would write to matching albums, writing
    /// nothing. Albums with nothing to credit are left out.
    pub async fn credit_plan(
        &self,
        query: &[String],
        known: &sift::credits::Known,
    ) -> anyhow::Result<Vec<CreditedAlbum>> {
        let mut out = Vec::new();
        for a in self.read(query).await? {
            let plan = match self.tagger.credit_plan(&a.dir, known).await {
                Ok(p) => p,
                Err(e) => sift::CreditPlan {
                    problems: vec![format!("{e:#}")],
                    ..Default::default()
                },
            };
            if plan.files.is_empty() && plan.problems.is_empty() {
                continue;
            }
            let mut artists: Vec<String> = Vec::new();
            for c in &plan.files {
                for a in c.artists.iter().chain(&c.album_artists) {
                    if !artists.contains(a) {
                        artists.push(a.clone());
                    }
                }
            }
            out.push(CreditedAlbum {
                path: a.dir.to_string_lossy().into_owned(),
                files: plan.files.len(),
                artists,
                planned: plan.files.iter().map(CreditWrite::from).collect(),
                problems: plan.problems,
            });
        }
        Ok(out)
    }

    /// What enriching the album at `dir` would add, writing nothing.
    pub async fn enrich_plan(
        &self,
        dir: &str,
        known: &sift::credits::Known,
    ) -> anyhow::Result<sift::EnrichPlan> {
        Ok(self.tagger.enrich_plan(Path::new(dir), known).await?)
    }

    /// The files of the album at `dir` and the tags each carries, read from
    /// the files now rather than the index.
    pub async fn tracks(&self, dir: &str) -> anyhow::Result<Vec<sift::meta::Track>> {
        let mut tracks = self.tagger.tracks(Path::new(dir)).await?;
        tracks.sort_by_key(|t| (t.disc, t.track, t.path.clone()));
        Ok(tracks)
    }

    /// Every field re-tagging the album at `dir` from `release_id` would
    /// change, file by file, and where each file would go. Changes nothing.
    pub async fn retag_plan(&self, dir: &str, release_id: &str) -> anyhow::Result<sift::Retag> {
        Ok(self.tagger.retag_plan(Path::new(dir), release_id).await?)
    }

    /// What `edits` would change in each of `tracks`, the files of the
    /// album at `dir` as just read, and where the edited tags would file
    /// it. Changes nothing.
    pub async fn edit_plan(
        &self,
        dir: &str,
        tracks: &[sift::meta::Track],
        edits: &TagEdits,
    ) -> anyhow::Result<EditPlan> {
        check_editable(edits)?;
        let edited: Vec<sift::meta::Track> = tracks
            .iter()
            .map(|t| {
                let mut e = t.clone();
                e.apply(&edits.for_file(&t.path));
                e
            })
            .collect();
        let files = tracks
            .iter()
            .zip(&edited)
            .map(|(t, e)| (t.path.clone(), sift::meta::diff(t, e)))
            .collect();
        let Some(mut album) = self
            .read(&[format!("path:={dir}")])
            .await?
            .into_iter()
            .next()
        else {
            anyhow::bail!("{dir} is no longer in the library");
        };
        // The move is planned from the tags just read, edited, rather than
        // the index's copy of them.
        for item in &mut album.items {
            if let Some(e) = edited.iter().find(|e| e.path == item.track.path) {
                item.track = e.clone();
            } else {
                let changes = edits.for_file(&item.track.path);
                item.track.apply(&changes);
            }
        }
        let refile = match tokio::task::block_in_place(|| manage::plan_move(&self.cfg, &album)) {
            Plan::InPlace => Preview::InPlace,
            Plan::Refused(why) => Preview::Refused(why),
            Plan::Moves(moves) => {
                Preview::Moves(moves.into_iter().map(|m| (m.from, m.to)).collect())
            }
        };
        Ok(EditPlan { files, refile })
    }

    /// Write `edits` to the album at `dir`, then re-file it where its new
    /// tags put it: moved (`to`), left with the reason (`refused`), or in
    /// place (neither).
    pub async fn edit(&self, dir: &str, edits: &TagEdits) -> anyhow::Result<AlbumMove> {
        let mut moves = self
            .edit_many(&[AlbumEdit {
                dir: dir.to_string(),
                edits: edits.clone(),
                read: None,
            }])
            .await?;
        Ok(moves.remove(0))
    }

    /// [`Self::edit`] for several albums at once, all or nothing: every album
    /// and file is checked before anything is written, and if a write fails,
    /// the files already written get their edited fields back as they read.
    /// Then the albums are re-filed together; one that cannot be is reported
    /// as refused, its tags already written.
    pub async fn edit_many(&self, albums: &[AlbumEdit]) -> anyhow::Result<Vec<AlbumMove>> {
        check_batch(albums)?;
        {
            let _index = self.write().await?;
            // Each file to write: its changes, the fields as they read, and
            // whether writing them drops MusicBrainz artist ids an undo
            // cannot put back.
            let mut writes = Vec::new();
            for a in albums {
                let dir = &a.dir;
                if self.read(&[format!("path:={dir}")]).await?.is_empty() {
                    anyhow::bail!("{dir} is not an album in the library");
                }
                let tracks = self.tagger.tracks(Path::new(dir)).await?;
                if let Some((p, _)) = a
                    .edits
                    .tracks
                    .iter()
                    .find(|(p, _)| !tracks.iter().any(|t| &t.path == p))
                {
                    anyhow::bail!("{} is not a file of {dir}", p.display());
                }
                for t in tracks {
                    let changes = a.edits.for_file(&t.path);
                    if changes.is_empty() {
                        continue;
                    }
                    if let Some(read) = &a.read
                        && read
                            .iter()
                            .find(|r| r.path == t.path)
                            .is_none_or(|r| !sift::meta::diff(r, &t).is_empty())
                    {
                        anyhow::bail!(
                            "{} changed since it was read; read it again",
                            t.path.display()
                        );
                    }
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&t.path)
                        .map_err(|e| {
                            anyhow::anyhow!("{} cannot be written: {e}", t.path.display())
                        })?;
                    let undo: Vec<_> = changes
                        .iter()
                        .map(|(f, _)| (f.clone(), t.field(f)))
                        .collect();
                    let drops_ids = changes.iter().any(|(f, _)| {
                        (f == "artists" && !t.mb_artist_ids.is_empty())
                            || (f == "albumartists" && !t.mb_album_artist_ids.is_empty())
                    });
                    writes.push((t.path, changes, undo, drops_ids));
                }
            }
            for (i, (path, changes, ..)) in writes.iter().enumerate() {
                let Err(e) = tokio::task::block_in_place(|| sift::meta::set(path, changes)) else {
                    continue;
                };
                let mut stuck = Vec::new();
                for (p, _, undo, _) in writes[..i].iter().rev() {
                    if let Err(u) = tokio::task::block_in_place(|| sift::meta::set(p, undo)) {
                        tracing::error!(file = %p.display(), error = %u, "could not undo an edit");
                        stuck.push(p.display().to_string());
                    }
                }
                self.bump();
                let mut outcome = if i == 0 {
                    "nothing was written".to_string()
                } else if stuck.is_empty() {
                    "the files already written were put back as they read".to_string()
                } else {
                    format!("these keep the edit: {}", stuck.join(", "))
                };
                if writes[..i].iter().any(|w| w.3) {
                    outcome.push_str(
                        "; MusicBrainz artist ids cleared with a credit list were not restored",
                    );
                }
                return Err(
                    anyhow::Error::new(e).context(format!("writing {}; {outcome}", path.display()))
                );
            }
            self.bump();
        }
        tracing::info!(albums = ?albums, "edited tags");
        let dirs: Vec<&str> = albums.iter().map(|a| a.dir.as_str()).collect();
        let mut moves = Vec::new();
        let refiled = self
            .refile_each(&by_path(dirs.iter().copied()), |m| moves.push(m))
            .await;
        Ok(dirs
            .into_iter()
            .map(|dir| {
                moves
                    .iter()
                    .position(|m| m.from == dir)
                    .map(|i| moves.swap_remove(i))
                    .unwrap_or_else(|| AlbumMove {
                        from: dir.to_string(),
                        to: None,
                        refused: refiled
                            .as_ref()
                            .err()
                            .map(|e| format!("tags written; not re-filed: {e:#}")),
                        files: 0,
                    })
            })
            .collect())
    }

    /// How the album at `dir` lines up against one release. Changes nothing.
    pub async fn compare(&self, dir: &str, release_id: &str) -> anyhow::Result<sift::Comparison> {
        Ok(self.tagger.compare(Path::new(dir), release_id).await?)
    }

    /// The release re-importing the album at `dir` would match, and how its
    /// files line up against it. Changes nothing.
    pub async fn rematch(&self, dir: &str) -> anyhow::Result<sift::Rematch> {
        Ok(self.tagger.rematch(Path::new(dir)).await?)
    }

    /// Match the album at `dir` against MusicBrainz again and retag it,
    /// moving it if the new tags file it elsewhere. With `release_id`, that
    /// release is applied, as when a person answers a review; without, the
    /// closest is applied only when it is close and complete.
    pub async fn reimport(
        &self,
        dir: &str,
        release_id: Option<&str>,
    ) -> anyhow::Result<ReimportedAlbum> {
        // Held so no other change moves files while this one does.
        let mut index = self.write().await?;
        let added = self
            .read(&[format!("path:={dir}")])
            .await?
            .first()
            .and_then(|a| a.items.iter().map(|i| i.added).min());
        let outcome = self.tagger.reimport(Path::new(dir), release_id).await;
        self.bump();
        let mut out = ReimportedAlbum {
            path: dir.to_string(),
            release: None,
            review: None,
            to: None,
            problems: Vec::new(),
        };
        match outcome? {
            sift::Outcome::Imported {
                dir: to, release, ..
            } => {
                tracing::info!(from = dir, to = %to.display(), "re-imported an album");
                // Moved files are new to a scan; the album keeps the date it
                // first arrived, so it does not jump to the top of the list.
                self.refresh(&mut index)?;
                if let Some(added) = added {
                    index.set_added(&to, added)?;
                }
                out.to = Some(to.to_string_lossy().into_owned());
                out.release = release.as_ref().map(ReleaseMatch::from);
            }
            sift::Outcome::Review {
                reason, candidates, ..
            } => {
                out.review = Some(reason);
                out.release = candidates.first().map(ReleaseMatch::from);
            }
        }
        Ok(out)
    }

    /// Albums not where the current path rules put them. With `apply`, each
    /// is moved; albums whose plan collides with anything are left alone.
    /// Set or clear tags on every file of the albums `query` matches, then
    /// re-file those the change moves. `changes` are beets' `field=value` and
    /// `field!`.
    pub async fn modify(
        &self,
        query: &[String],
        changes: &[String],
    ) -> anyhow::Result<(usize, Vec<AlbumMove>)> {
        let (stray, changes) =
            manage::split_modify_args(changes).map_err(|e| anyhow::anyhow!(e))?;
        if !stray.is_empty() {
            anyhow::bail!(
                "not a change: {}; changes are field=value or field!",
                stray.join(" ")
            );
        }
        if changes.is_empty() {
            anyhow::bail!("no changes given");
        }
        let mut index = self.write().await?;
        let report = manage::modify(
            &self.cfg,
            &mut index,
            &Query::parse(query)?,
            true,
            &changes,
            false,
        )
        .await?;
        self.bump();
        let mut moves: Vec<AlbumMove> = report
            .moved
            .into_iter()
            .map(|(from, to)| AlbumMove {
                from: from.to_string_lossy().into_owned(),
                to: Some(to.to_string_lossy().into_owned()),
                refused: None,
                files: 0,
            })
            .collect();
        moves.extend(report.left.into_iter().map(|(from, why)| AlbumMove {
            from: from.to_string_lossy().into_owned(),
            to: None,
            refused: Some(why),
            files: 0,
        }));
        tracing::info!(files = report.files.len(), query = ?query, "modified tags");
        Ok((report.files.len(), moves))
    }

    pub async fn refile(&self, query: &[String], apply: bool) -> anyhow::Result<Vec<AlbumMove>> {
        if !apply {
            if query.is_empty() {
                return Ok(self.overview().await?.plan.clone());
            }
            let albums = self.read(query).await?;
            return Ok(tokio::task::block_in_place(|| self.plan_of(&albums)));
        }
        let mut out = Vec::new();
        self.refile_each(query, |m| {
            if m.to.is_some() || m.refused.is_some() {
                out.push(m);
            }
        })
        .await?;
        Ok(out)
    }

    /// Move each album `query` matches to where the rules file it, telling
    /// `done` about each as it goes: moved (`to`), left with the reason
    /// (`refused`), or already in place (neither). The index is brought up
    /// to date once, before the first; each album is planned again as it
    /// moves, so nothing acts on a plan older than that.
    pub async fn refile_each(
        &self,
        query: &[String],
        mut done: impl FnMut(AlbumMove),
    ) -> anyhow::Result<()> {
        let mut index = self.write().await?;
        let albums = tokio::task::block_in_place(|| index.albums(&Query::parse(query)?))?;
        for album in albums {
            let from = album.dir.to_string_lossy().into_owned();
            match tokio::task::block_in_place(|| manage::plan_move(&self.cfg, &album)) {
                Plan::InPlace => done(AlbumMove {
                    from,
                    to: None,
                    refused: None,
                    files: 0,
                }),
                Plan::Refused(why) => done(AlbumMove {
                    from,
                    to: None,
                    refused: Some(why),
                    files: 0,
                }),
                Plan::Moves(moves) => {
                    let to = moves
                        .first()
                        .and_then(|m| m.to.parent())
                        .map(|p| p.to_string_lossy().into_owned());
                    manage::execute(&mut index, &album, &moves).await?;
                    self.bump();
                    tracing::info!(from, to = ?to, "re-filed an album");
                    done(AlbumMove {
                        from,
                        to,
                        refused: None,
                        files: moves.len(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Exactly what re-filing the album at `query` would do now: every file
    /// it would move, from and to, or why it would be left.
    pub async fn preview(&self, query: &[String]) -> anyhow::Result<Option<Preview>> {
        let Some(album) = self.read(query).await?.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(
            match tokio::task::block_in_place(|| manage::plan_move(&self.cfg, &album)) {
                Plan::InPlace => Preview::InPlace,
                Plan::Refused(why) => Preview::Refused(why),
                Plan::Moves(moves) => {
                    Preview::Moves(moves.into_iter().map(|m| (m.from, m.to)).collect())
                }
            },
        ))
    }
}
