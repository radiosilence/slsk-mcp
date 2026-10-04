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
    pub problems: Vec<String>,
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
    async fn overview(&self) -> anyhow::Result<Arc<Overview>> {
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
                let to = manage::bin(&mut index, &self.cfg.directory, &self.bin, album).await?;
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

    /// Add gain, genres and lyrics to matching albums, as every new import
    /// gets. Rewrites tags (only those fields) across what the query
    /// matches, so it is for a deliberate backfill.
    pub async fn enrich(&self, query: &[String]) -> anyhow::Result<Vec<EnrichedAlbum>> {
        let mut out = Vec::new();
        for a in self.read(query).await? {
            let e = self.tagger.enrich(&a.dir).await?;
            out.push(EnrichedAlbum {
                path: a.dir.to_string_lossy().into_owned(),
                gain_db: e.gain_db,
                genres: e.genres,
                lyrics: e.lyrics,
                problems: e.problems,
            });
        }
        self.changed();
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
