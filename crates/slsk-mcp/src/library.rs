//! The library as a whole, through sift's index: listing it, re-filing it
//! under the current path rules, and moving spare copies of albums to a bin
//! beside it.
//!
//! The bin is outside the library directory, so neither Navidrome nor the
//! shares see what is in it, and on the same drive, so binning is a rename.
//! Nothing here deletes a file.

use std::path::{Path, PathBuf};

use async_graphql::SimpleObject;
use sift::library::{Album, Query, Value};
use sift::manage::{self, Plan};
use tokio::sync::Mutex;

pub struct Library {
    index: Mutex<sift::library::Library>,
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

#[derive(SimpleObject)]
pub struct SpareCopy {
    pub album: LibraryAlbum,
    /// Why this copy is the spare: "lossy copy of a lossless album", …
    pub reason: String,
    /// Where it went, once binned.
    pub binned_to: Option<String>,
}

#[derive(SimpleObject)]
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

#[derive(SimpleObject)]
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
            index: Mutex::new(sift::library::Library::open(index)?.with_workers(2)),
            cfg: tagger.cfg.clone(),
            bin,
            tagger,
        })
    }

    pub fn root(&self) -> &Path {
        &self.cfg.directory
    }

    /// Bring the index up to date with the files. Incremental: only files
    /// whose size or modification time changed are read.
    fn refresh(&self, index: &mut sift::library::Library) -> anyhow::Result<()> {
        let report = tokio::task::block_in_place(|| index.update(&self.cfg.directory))?;
        if report.added + report.changed + report.removed > 0 {
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

    /// Index the library at startup, so the first query does not wait for a
    /// full read of it.
    pub async fn warm(&self) {
        let mut index = self.index.lock().await;
        if let Err(e) = self.refresh(&mut index) {
            tracing::warn!(error = %e, "library index update failed");
        }
    }

    pub async fn albums(&self, query: &[String]) -> anyhow::Result<Vec<LibraryAlbum>> {
        let mut index = self.index.lock().await;
        self.refresh(&mut index)?;
        Ok(index
            .albums(&Query::parse(query)?)?
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
        let mut index = self.index.lock().await;
        self.refresh(&mut index)?;
        let albums = index.albums(&Query::parse(query)?)?;
        let mut out = Vec::new();
        for d in manage::duplicates(&self.cfg, &albums) {
            let mut spares = Vec::new();
            for (album, reason) in &d.others {
                let binned_to = if bin {
                    let to = manage::bin(&mut index, &self.cfg.directory, &self.bin, album).await?;
                    tracing::info!(
                        from = %album.dir.display(),
                        to = %to.display(),
                        reason,
                        "binned a spare copy"
                    );
                    Some(to.to_string_lossy().into_owned())
                } else {
                    None
                };
                spares.push(SpareCopy {
                    album: LibraryAlbum::from(*album),
                    reason: reason.to_string(),
                    binned_to,
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
        let albums = {
            let mut index = self.index.lock().await;
            self.refresh(&mut index)?;
            index.albums(&Query::parse(query)?)?
        };
        let mut out = Vec::new();
        for a in albums {
            let e = self.tagger.enrich(&a.dir).await?;
            out.push(EnrichedAlbum {
                path: a.dir.to_string_lossy().into_owned(),
                gain_db: e.gain_db,
                genres: e.genres,
                lyrics: e.lyrics,
                problems: e.problems,
            });
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
        let mut index = self.index.lock().await;
        self.refresh(&mut index)?;
        let report = manage::modify(
            &self.cfg,
            &mut index,
            &Query::parse(query)?,
            true,
            &changes,
            false,
        )
        .await?;
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
        let mut index = self.index.lock().await;
        self.refresh(&mut index)?;
        let mut out = Vec::new();
        for album in index.albums(&Query::parse(query)?)? {
            let from = album.dir.to_string_lossy().into_owned();
            match manage::plan_move(&self.cfg, &album) {
                Plan::InPlace => {}
                Plan::Refused(why) => out.push(AlbumMove {
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
                    if apply {
                        manage::execute(&mut index, &album, &moves).await?;
                        tracing::info!(from, to = ?to, "re-filed an album");
                    }
                    out.push(AlbumMove {
                        from,
                        to,
                        refused: None,
                        files: moves.len(),
                    });
                }
            }
        }
        Ok(out)
    }
}
