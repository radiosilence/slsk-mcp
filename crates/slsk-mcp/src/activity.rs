//! What other users do with our shares: who browses them and which folders
//! they open, which searches reach us and whether our shares answer them,
//! and the uploads that follow. Shown as it happens and kept for trends.
//!
//! Browses, opened folders and matched searches are kept a row each. Every
//! search is also counted by its normalised words per hour; the distributed
//! network relays many searches a second, so the counts are held in memory
//! for the hour and only its most frequent words are written when it ends.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Timelike, Utc};
use rusqlite::params;
use slsk_engine::{Event, TransferView};

use crate::db::{Db, ts_from};
use crate::live::Topic;
use crate::session::Session;

/// Events shown in the live feed.
const FEED: usize = 200;
/// Words kept per hour, most searched first.
const KEEP_TERMS: usize = 1000;
/// Distinct words held for the hour before the once-searched are dropped.
const MAX_TERMS: usize = 100_000;
/// How long kept events and counts last.
const KEEP_DAYS: i64 = 90;
/// How often new rows are written, in one transaction.
const FLUSH: Duration = Duration::from_secs(5);

/// One thing someone did with our shares.
#[derive(Debug, Clone, PartialEq, async_graphql::SimpleObject)]
pub struct ShareEvent {
    pub at: DateTime<Utc>,
    /// browse, folder, search, upload (started), uploaded (finished),
    /// upload_failed.
    pub kind: String,
    pub username: String,
    /// The folder opened, the search's words, or the file uploaded.
    pub detail: Option<String>,
    /// Files a search matched.
    pub results: Option<u32>,
    /// A search relayed by the distributed network rather than the server.
    pub distributed: bool,
}

/// Searches counted over a window.
#[derive(Debug, Clone, Default, PartialEq, async_graphql::SimpleObject)]
pub struct Term {
    /// The search's words, normalised: lower case, punctuation dropped.
    pub term: String,
    pub searches: u64,
    /// Of those, the searches our shares answered.
    pub matched: u64,
}

#[derive(Debug, Clone, PartialEq, async_graphql::SimpleObject)]
pub struct FolderInterest {
    pub folder: String,
    /// Times someone opened it while browsing.
    pub opens: u64,
    /// Files from it uploaded in full.
    pub downloads: u64,
}

#[derive(Debug, Clone, PartialEq, async_graphql::SimpleObject)]
pub struct Visitor {
    pub username: String,
    /// Distinct days they browsed, searched our shares or downloaded.
    pub days: u64,
    pub events: u64,
    pub last: DateTime<Utc>,
}

/// What people have been after over the last `days`.
#[derive(Debug, Clone, PartialEq, async_graphql::SimpleObject)]
pub struct Trends {
    pub days: u32,
    /// Searches that reached us, those our shares answered, and those of
    /// them relayed by the distributed network.
    pub searches: u64,
    pub matched: u64,
    pub distributed: u64,
    /// Most searched words.
    pub searched: Vec<Term>,
    /// Most searched words our shares never answered: what to look for.
    pub missing: Vec<Term>,
    /// Our folders most opened or downloaded from.
    pub folders: Vec<FolderInterest>,
    /// People who came back on more than one day.
    pub visitors: Vec<Visitor>,
}

#[derive(Default)]
struct Hour {
    start: Option<DateTime<Utc>>,
    terms: HashMap<String, (u64, u64)>,
    searches: u64,
    matched: u64,
    distributed: u64,
}

pub struct Activity {
    db: Db,
    feed: parking_lot::Mutex<VecDeque<ShareEvent>>,
    hour: parking_lot::Mutex<Hour>,
    /// Rows not yet written.
    pending: parking_lot::Mutex<Vec<ShareEvent>>,
}

/// A search's words as they are counted: lower case, letters and digits
/// only, single spaces. Empty when nothing is left.
pub fn normalise(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    words.truncate(16);
    words.join(" ").chars().take(120).collect()
}

/// A count read back from SQLite, which stores integers signed.
fn count(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
    Ok(r.get::<_, i64>(i)?.max(0) as u64)
}

fn stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3f+00:00").to_string()
}

fn hour_of(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t)
}

/// The folder a share path names its file in.
fn folder_of(path: &str) -> &str {
    path.rsplit_once('\\').map_or(path, |(f, _)| f)
}

impl Activity {
    pub fn new(db: Db) -> Arc<Self> {
        Arc::new(Self {
            db,
            feed: Default::default(),
            hour: Default::default(),
            pending: Default::default(),
        })
    }

    /// Follow the engine's activity for as long as the process runs, from
    /// whenever an account first appears.
    pub fn spawn(self: &Arc<Self>, session: Arc<Session>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut changes = slsk_engine::changes();
            let engine = loop {
                if let Some(e) = session.engine() {
                    break e.clone();
                }
                if changes.changed().await.is_err() {
                    return;
                }
            };
            let mut events = engine.events();
            let mut flush = tokio::time::interval(FLUSH);
            loop {
                tokio::select! {
                    e = events.recv() => match e {
                        Ok(Event::Activity(a)) => me.on(&a, Utc::now()),
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(n, "share activity lagged; those searches are not counted")
                        }
                        Err(_) => return,
                    },
                    _ = flush.tick() => {
                        if let Err(e) = me.flush(Utc::now()).await {
                            tracing::warn!(error = %e, "could not record share activity");
                        }
                    }
                }
            }
        });
    }

    fn on(&self, a: &slsk_engine::Activity, at: DateTime<Utc>) {
        use slsk_engine::Activity as A;
        let event = match a {
            A::Browse { username } => ShareEvent {
                at,
                kind: "browse".into(),
                username: username.clone(),
                detail: None,
                results: None,
                distributed: false,
            },
            A::Folder { username, folder } => ShareEvent {
                at,
                kind: "folder".into(),
                username: username.clone(),
                detail: Some(folder.clone()),
                results: None,
                distributed: false,
            },
            A::Search {
                username,
                query,
                distributed,
                results,
            } => {
                self.count(query, *results > 0, *distributed, at);
                if *results == 0 {
                    return;
                }
                ShareEvent {
                    at,
                    kind: "search".into(),
                    username: username.clone(),
                    detail: Some(query.clone()),
                    results: Some(*results as u32),
                    distributed: *distributed,
                }
            }
        };
        self.pending.lock().push(event.clone());
        self.show(event);
    }

    /// An upload starting, or ending as completed, failed or cancelled.
    pub fn upload(&self, t: &TransferView, started: bool) {
        let kind = match (started, t.state) {
            (true, _) => "upload",
            (false, "completed") => "uploaded",
            (false, _) => "upload_failed",
        };
        self.show(ShareEvent {
            at: Utc::now(),
            kind: kind.into(),
            username: t.username.clone(),
            detail: Some(t.filename.to_string_lossy()),
            results: None,
            distributed: false,
        });
    }

    fn show(&self, event: ShareEvent) {
        let mut feed = self.feed.lock();
        if feed.len() == FEED {
            feed.pop_back();
        }
        feed.push_front(event);
        drop(feed);
        crate::live::changed(Topic::Activity);
    }

    fn count(&self, query: &str, matched: bool, distributed: bool, at: DateTime<Utc>) {
        let mut hour = self.hour.lock();
        let start = hour_of(at);
        if hour.start.is_none() {
            hour.start = Some(start);
        }
        hour.searches += 1;
        hour.matched += u64::from(matched);
        hour.distributed += u64::from(distributed);
        let term = normalise(query);
        if term.is_empty() {
            return;
        }
        if hour.terms.len() >= MAX_TERMS && !hour.terms.contains_key(&term) {
            hour.terms.retain(|_, (n, _)| *n > 1);
            if hour.terms.len() >= MAX_TERMS {
                return;
            }
        }
        let entry = hour.terms.entry(term).or_default();
        entry.0 += 1;
        entry.1 += u64::from(matched);
    }

    /// The newest of what happened, newest first.
    pub fn feed(&self, limit: usize) -> Vec<ShareEvent> {
        self.feed.lock().iter().take(limit).cloned().collect()
    }

    /// Searches this hour so far: all, matched, distributed.
    pub fn this_hour(&self) -> (u64, u64, u64) {
        let hour = self.hour.lock();
        (hour.searches, hour.matched, hour.distributed)
    }

    /// Write what is pending; once the hour has turned, its counts too, and
    /// prune what is past keeping.
    async fn flush(&self, now: DateTime<Utc>) -> Result<()> {
        let rows = std::mem::take(&mut *self.pending.lock());
        let ended = {
            let mut hour = self.hour.lock();
            match hour.start {
                Some(start) if hour_of(now) > start => Some(std::mem::take(&mut *hour)),
                _ => None,
            }
        };
        if rows.is_empty() && ended.is_none() {
            return Ok(());
        }
        self.db
            .run(move |c| {
                let tx = c.transaction()?;
                {
                    let mut insert = tx.prepare_cached(
                        "INSERT INTO share_events (at, kind, username, detail, results) VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?;
                    for e in &rows {
                        insert.execute(params![stamp(e.at), e.kind, e.username, e.detail, e.results])?;
                    }
                }
                if let Some(hour) = ended
                    && let Some(start) = hour.start
                {
                    let key = start.format("%Y-%m-%dT%H").to_string();
                    tx.execute(
                        "INSERT INTO search_hours (hour, searches, matched, distributed) VALUES (?1, ?2, ?3, ?4) \
                         ON CONFLICT (hour) DO UPDATE SET searches = searches + excluded.searches, \
                         matched = matched + excluded.matched, distributed = distributed + excluded.distributed",
                        params![key, hour.searches as i64, hour.matched as i64, hour.distributed as i64],
                    )?;
                    let mut terms: Vec<_> = hour.terms.into_iter().collect();
                    terms.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
                    terms.truncate(KEEP_TERMS);
                    let mut insert = tx.prepare_cached(
                        "INSERT INTO search_terms (hour, term, searches, matched) VALUES (?1, ?2, ?3, ?4) \
                         ON CONFLICT (hour, term) DO UPDATE SET searches = searches + excluded.searches, \
                         matched = matched + excluded.matched",
                    )?;
                    for (term, (searches, matched)) in terms {
                        insert.execute(params![key, term, searches as i64, matched as i64])?;
                    }
                    drop(insert);
                    let cutoff = now - chrono::Duration::days(KEEP_DAYS);
                    tx.execute("DELETE FROM share_events WHERE at < ?1", [stamp(cutoff)])?;
                    let hour_cutoff = cutoff.format("%Y-%m-%dT%H").to_string();
                    tx.execute("DELETE FROM search_terms WHERE hour < ?1", [&hour_cutoff])?;
                    tx.execute("DELETE FROM search_hours WHERE hour < ?1", [&hour_cutoff])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Kept events, newest first, of one kind or all.
    pub async fn log(&self, kind: Option<String>, limit: u32) -> Result<Vec<ShareEvent>> {
        Ok(self
            .db
            .run(move |c| {
                let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<ShareEvent> {
                    Ok(ShareEvent {
                        at: ts_from(&r.get::<_, String>(0)?)?,
                        kind: r.get(1)?,
                        username: r.get(2)?,
                        detail: r.get(3)?,
                        results: r.get(4)?,
                        distributed: false,
                    })
                };
                const COLUMNS: &str =
                    "SELECT at, kind, username, detail, results FROM share_events";
                match kind {
                    Some(k) => c
                        .prepare_cached(&format!(
                            "{COLUMNS} WHERE kind = ?1 ORDER BY at DESC LIMIT ?2"
                        ))?
                        .query_map(params![k, limit], map)?
                        .collect(),
                    None => c
                        .prepare_cached(&format!("{COLUMNS} ORDER BY at DESC LIMIT ?1"))?
                        .query_map(params![limit], map)?
                        .collect(),
                }
            })
            .await?)
    }

    /// What people have been after over the last `days`, the hour in hand
    /// included.
    pub async fn trends(&self, days: u32) -> Result<Trends> {
        let since = Utc::now() - chrono::Duration::days(i64::from(days.clamp(1, KEEP_DAYS as u32)));
        let (since_at, since_hour) = (stamp(since), since.format("%Y-%m-%dT%H").to_string());
        let (mut totals, mut terms, folders, visitors) = self
            .db
            .run(move |c| {
                let totals: (u64, u64, u64) = c.query_row(
                    "SELECT COALESCE(SUM(searches), 0), COALESCE(SUM(matched), 0), COALESCE(SUM(distributed), 0) \
                     FROM search_hours WHERE hour >= ?1",
                    [&since_hour],
                    |r| Ok((count(r, 0)?, count(r, 1)?, count(r, 2)?)),
                )?;
                let terms: HashMap<String, (u64, u64)> = c
                    .prepare_cached(
                        "SELECT term, SUM(searches), SUM(matched) FROM search_terms WHERE hour >= ?1 \
                         GROUP BY term ORDER BY SUM(searches) DESC LIMIT 2000",
                    )?
                    .query_map([&since_hour], |r| Ok((r.get(0)?, (count(r, 1)?, count(r, 2)?))))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut folders: HashMap<String, (u64, u64)> = HashMap::new();
                for row in c
                    .prepare_cached(
                        "SELECT detail, COUNT(*) FROM share_events WHERE kind = 'folder' AND at >= ?1 GROUP BY detail",
                    )?
                    .query_map([&since_at], |r| Ok((r.get::<_, String>(0)?, count(r, 1)?)))?
                {
                    let (folder, n) = row?;
                    folders.entry(folder).or_default().0 += n;
                }
                for row in c
                    .prepare_cached(
                        "SELECT filename FROM uploads WHERE state = 'completed' AND finished_at >= ?1 \
                         ORDER BY finished_at DESC LIMIT 20000",
                    )?
                    .query_map([&since_at], |r| r.get::<_, String>(0))?
                {
                    let file = row?;
                    folders.entry(folder_of(&file).to_string()).or_default().1 += 1;
                }
                let visitors: Vec<Visitor> = c
                    .prepare_cached(
                        "SELECT username, COUNT(DISTINCT substr(at, 1, 10)) AS days, COUNT(*), MAX(at) FROM ( \
                           SELECT lower(username) AS username, at FROM share_events WHERE at >= ?1 \
                           UNION ALL SELECT lower(username), finished_at FROM uploads WHERE finished_at >= ?1 \
                         ) GROUP BY username HAVING days > 1 ORDER BY days DESC, COUNT(*) DESC LIMIT 20",
                    )?
                    .query_map([&since_at], |r| {
                        Ok(Visitor {
                            username: r.get(0)?,
                            days: count(r, 1)?,
                            events: count(r, 2)?,
                            last: ts_from(&r.get::<_, String>(3)?)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                Ok((totals, terms, folders, visitors))
            })
            .await?;
        {
            let hour = self.hour.lock();
            totals.0 += hour.searches;
            totals.1 += hour.matched;
            totals.2 += hour.distributed;
            for (term, (n, m)) in &hour.terms {
                let e = terms.entry(term.clone()).or_default();
                e.0 += n;
                e.1 += m;
            }
        }
        let mut terms: Vec<Term> = terms
            .into_iter()
            .map(|(term, (searches, matched))| Term {
                term,
                searches,
                matched,
            })
            .collect();
        terms.sort_by(|a, b| {
            b.searches
                .cmp(&a.searches)
                .then_with(|| a.term.cmp(&b.term))
        });
        let missing = terms
            .iter()
            .filter(|t| t.matched == 0 && t.searches > 1)
            .take(20)
            .cloned()
            .collect();
        terms.truncate(20);
        let mut folders: Vec<FolderInterest> = folders
            .into_iter()
            .map(|(folder, (opens, downloads))| FolderInterest {
                folder,
                opens,
                downloads,
            })
            .collect();
        folders.sort_by(|a, b| {
            (b.downloads + b.opens)
                .cmp(&(a.downloads + a.opens))
                .then_with(|| a.folder.cmp(&b.folder))
        });
        folders.truncate(20);
        Ok(Trends {
            days,
            searches: totals.0,
            matched: totals.1,
            distributed: totals.2,
            searched: terms,
            missing,
            folders,
            visitors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_is_counted_by_its_words_alone() {
        assert_eq!(
            normalise("Boards of Canada - Geogaddi (2002) [FLAC]"),
            "boards of canada geogaddi 2002 flac"
        );
        assert_eq!(normalise("  ÀÉ!!  "), "àé");
        assert_eq!(normalise("--"), "");
    }

    #[tokio::test]
    async fn matched_searches_are_kept_and_every_search_counted_by_the_hour() {
        let db = Db::memory().await;
        let a = Activity::new(db);
        let t = Utc::now() - chrono::Duration::hours(1);
        let search = |q: &str, results| slsk_engine::Activity::Search {
            username: "x".into(),
            query: q.into(),
            distributed: true,
            results,
        };
        a.on(&search("Geogaddi", 3), t);
        a.on(&search("geogaddi!", 0), t);
        a.on(&search("Missing Album", 0), t);
        a.on(&search("missing album", 0), t);
        a.on(
            &slsk_engine::Activity::Folder {
                username: "y".into(),
                folder: "Music\\BoC\\Geogaddi".into(),
            },
            t,
        );
        assert_eq!(a.feed(10).len(), 2, "the matched search and the folder");
        a.flush(Utc::now()).await.unwrap();
        let trends = a.trends(1).await.unwrap();
        assert_eq!(
            (trends.searches, trends.matched, trends.distributed),
            (4, 1, 4)
        );
        assert_eq!(trends.searched[0].searches, 2);
        assert_eq!(
            trends.missing,
            [Term {
                term: "missing album".into(),
                searches: 2,
                matched: 0
            }]
        );
        assert_eq!(trends.folders[0].folder, "Music\\BoC\\Geogaddi");
        assert_eq!(a.log(Some("search".into()), 10).await.unwrap().len(), 1);
    }

    #[test]
    fn an_upload_names_its_folder() {
        assert_eq!(folder_of("Music\\A\\B\\01.flac"), "Music\\A\\B");
        assert_eq!(folder_of("loose.flac"), "loose.flac");
    }
}
