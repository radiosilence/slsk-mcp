//! What other users do with our shares: who browses them and which folders
//! they open, which searches reach us and whether our shares answer them,
//! and the uploads that follow. Shown as it happens and kept for trends.
//!
//! Browses, opened folders and matched searches are kept a row each. Every
//! search is also counted by its normalised words per hour; the distributed
//! network relays many searches a second, so the counts are held in memory
//! for the hour and only its most frequent words are written when it ends.
//!
//! Trends read per-day rollups (`visitor_days`, `folder_days`, `term_days`)
//! that triggers keep as rows are written, so a week of trends sums a few
//! thousand rows however many searches arrived.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Timelike, Utc};
use rusqlite::params;
use slsk_engine::TransferView;

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
/// Words kept per day in `term_days`, most searched first.
const KEEP_DAY_TERMS: u32 = 2000;
/// How often new rows are written, in one transaction.
const FLUSH: Duration = Duration::from_secs(5);
/// Rows kept per user a minute; the rest still show on the feed.
const PER_USER_MINUTE: u32 = 30;

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
    /// Distinct days they browsed, opened a folder or were sent a file.
    /// Matched searches do not count: the distributed network relays them
    /// from everyone.
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
    /// Full of terms searched more than once: new ones are not counted
    /// until the next hour.
    saturated: bool,
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
    /// Hours ended and not yet written.
    ended: parking_lot::Mutex<Vec<Hour>>,
    /// Rows kept this minute per user: the minute, and how many.
    kept: parking_lot::Mutex<HashMap<String, (i64, u32)>>,
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

/// Searches, matched and relayed by the distributed network, in an hour.
type Totals = (u64, u64, u64);
/// Words with their searches and matched counts.
type Counted = Vec<(String, u64, u64)>;

impl Hour {
    /// What is written for the hour: its key, its totals, and its most
    /// searched words.
    fn top(&self) -> (String, Totals, Counted) {
        let key = self
            .start
            .map(|s| s.format("%Y-%m-%dT%H").to_string())
            .unwrap_or_default();
        (
            key,
            (self.searches, self.matched, self.distributed),
            self.most(KEEP_TERMS),
        )
    }

    /// The `n` most searched words, copying no others.
    fn most(&self, n: usize) -> Counted {
        let mut terms: Vec<(&String, &(u64, u64))> = self.terms.iter().collect();
        let by = |a: &(&String, &(u64, u64)), b: &(&String, &(u64, u64))| {
            b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0))
        };
        if terms.len() > n {
            terms.select_nth_unstable_by(n, by);
            terms.truncate(n);
        }
        terms.sort_by(by);
        terms
            .into_iter()
            .map(|(t, (s, m))| (t.clone(), *s, *m))
            .collect()
    }
}

impl Activity {
    pub fn new(db: Db) -> Arc<Self> {
        Arc::new(Self {
            db,
            feed: Default::default(),
            hour: Default::default(),
            pending: Default::default(),
            ended: Default::default(),
            kept: Default::default(),
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
            let mut events = engine.activity();
            let mut flush = tokio::time::interval(FLUSH);
            loop {
                tokio::select! {
                    e = events.recv() => match e {
                        Ok(a) => me.on(&a, Utc::now()),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(n, "share activity lagged; those searches are not counted")
                        }
                        Err(_) => return,
                    },
                    _ = flush.tick() => {
                        if let Err(e) = me.flush(Utc::now(), false).await {
                            tracing::warn!(error = %e, "could not record share activity; trying again");
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
        if self.keep(&event.username, at) {
            self.pending.lock().push(event.clone());
        }
        self.show(event);
    }

    /// Whether a row from `username` is kept: at most [`PER_USER_MINUTE`]
    /// a minute, so one peer cannot fill the log.
    fn keep(&self, username: &str, at: DateTime<Utc>) -> bool {
        let minute = at.timestamp() / 60;
        let mut kept = self.kept.lock();
        if kept.len() > 10_000 {
            kept.retain(|_, (m, _)| *m == minute);
        }
        let entry = kept.entry(username.to_string()).or_insert((minute, 0));
        if entry.0 != minute {
            *entry = (minute, 0);
        }
        entry.1 += 1;
        entry.1 <= PER_USER_MINUTE
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
        match hour.start {
            None => hour.start = Some(start),
            Some(s) if start > s => {
                let ended = std::mem::take(&mut *hour);
                self.ended.lock().push(ended);
                hour.start = Some(start);
            }
            _ => {}
        }
        hour.searches += 1;
        hour.matched += u64::from(matched);
        hour.distributed += u64::from(distributed);
        let term = normalise(query);
        if term.is_empty() {
            return;
        }
        if hour.terms.len() >= MAX_TERMS && !hour.terms.contains_key(&term) {
            if hour.saturated {
                return;
            }
            hour.terms.retain(|_, (n, _)| *n > 1);
            if hour.terms.len() >= MAX_TERMS {
                hour.saturated = true;
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

    /// Write what is pending and any hour that has ended, pruning what is
    /// past keeping; with `all`, the hour in hand too, as on shutdown. What
    /// fails to be written is kept for the next try.
    async fn flush(&self, now: DateTime<Utc>, all: bool) -> Result<()> {
        let rows = std::mem::take(&mut *self.pending.lock());
        let mut hours = std::mem::take(&mut *self.ended.lock());
        {
            let mut hour = self.hour.lock();
            let turned = hour.start.is_some_and(|start| hour_of(now) > start);
            if turned || (all && hour.start.is_some()) {
                hours.push(std::mem::take(&mut *hour));
            }
        }
        if rows.is_empty() && hours.is_empty() {
            return Ok(());
        }
        let written = {
            let rows = rows.clone();
            let tops: Vec<_> = hours.iter().map(Hour::top).collect();
            self.db
                .run(move |c| {
                    let tx = c.transaction()?;
                    {
                        let mut insert = tx.prepare_cached(
                            "INSERT INTO share_events (at, kind, username, detail, results, distributed) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        )?;
                        for e in &rows {
                            insert.execute(params![
                                stamp(e.at),
                                e.kind,
                                e.username,
                                e.detail,
                                e.results,
                                e.distributed
                            ])?;
                        }
                    }
                    for (key, totals, terms) in &tops {
                        tx.execute(
                            "INSERT INTO search_hours (hour, searches, matched, distributed) VALUES (?1, ?2, ?3, ?4) \
                             ON CONFLICT (hour) DO UPDATE SET searches = searches + excluded.searches, \
                             matched = matched + excluded.matched, distributed = distributed + excluded.distributed",
                            params![key, totals.0 as i64, totals.1 as i64, totals.2 as i64],
                        )?;
                        let mut insert = tx.prepare_cached(
                            "INSERT INTO search_terms (hour, term, searches, matched) VALUES (?1, ?2, ?3, ?4) \
                             ON CONFLICT (hour, term) DO UPDATE SET searches = searches + excluded.searches, \
                             matched = matched + excluded.matched",
                        )?;
                        for (term, searches, matched) in terms {
                            insert.execute(params![key, term, *searches as i64, *matched as i64])?;
                        }
                    }
                    if !tops.is_empty() {
                        let cutoff = now - chrono::Duration::days(KEEP_DAYS);
                        tx.execute("DELETE FROM share_events WHERE at < ?1", [stamp(cutoff)])?;
                        let hour_cutoff = cutoff.format("%Y-%m-%dT%H").to_string();
                        tx.execute("DELETE FROM search_terms WHERE hour < ?1", [&hour_cutoff])?;
                        tx.execute("DELETE FROM search_hours WHERE hour < ?1", [&hour_cutoff])?;
                        let day_cutoff = cutoff.format("%Y-%m-%d").to_string();
                        for table in ["visitor_days", "folder_days", "term_days"] {
                            tx.execute(&format!("DELETE FROM {table} WHERE day < ?1"), [&day_cutoff])?;
                        }
                        // Only yesterday and today can have grown past the
                        // limit since the last hour ended.
                        let recent = (now - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
                        tx.execute(
                            "DELETE FROM term_days WHERE (day, term) IN ( \
                               SELECT day, term FROM ( \
                                 SELECT day, term, row_number() OVER (PARTITION BY day ORDER BY searches DESC, term) AS n \
                                 FROM term_days WHERE day >= ?1 \
                               ) WHERE n > ?2 \
                             )",
                            params![recent, KEEP_DAY_TERMS],
                        )?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await
        };
        if let Err(e) = written {
            let mut pending = self.pending.lock();
            let newer = std::mem::replace(&mut *pending, rows);
            pending.extend(newer);
            self.ended.lock().splice(0..0, hours);
            return Err(e.into());
        }
        Ok(())
    }

    /// Write everything held, the hour in hand included: on shutdown.
    pub async fn close(&self) {
        if let Err(e) = self.flush(Utc::now(), true).await {
            tracing::warn!(error = %e, "could not record share activity on the way out");
        }
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
                        distributed: r.get(5)?,
                    })
                };
                const COLUMNS: &str =
                    "SELECT at, kind, username, detail, results, distributed FROM share_events";
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
        let days = days.clamp(1, KEEP_DAYS as u32);
        let since = Utc::now() - chrono::Duration::days(i64::from(days));
        let since_hour = since.format("%Y-%m-%dT%H").to_string();
        let since_day = since.format("%Y-%m-%d").to_string();
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
                        "SELECT term, SUM(searches), SUM(matched) FROM term_days WHERE day >= ?1 \
                         GROUP BY term ORDER BY SUM(searches) DESC LIMIT 2000",
                    )?
                    .query_map([&since_day], |r| Ok((r.get(0)?, (count(r, 1)?, count(r, 2)?))))?
                    .collect::<rusqlite::Result<_>>()?;
                let folders: Vec<FolderInterest> = c
                    .prepare_cached(
                        "SELECT folder, SUM(opens), SUM(downloads) FROM folder_days WHERE day >= ?1 \
                         GROUP BY folder ORDER BY SUM(opens) + SUM(downloads) DESC, folder LIMIT 20",
                    )?
                    .query_map([&since_day], |r| {
                        Ok(FolderInterest {
                            folder: r.get(0)?,
                            opens: count(r, 1)?,
                            downloads: count(r, 2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let visitors: Vec<Visitor> = c
                    .prepare_cached(
                        "SELECT MAX(username), COUNT(*) AS days, SUM(events), MAX(last) FROM visitor_days \
                         WHERE day >= ?1 GROUP BY user_key HAVING days > 1 \
                         ORDER BY days DESC, SUM(events) DESC LIMIT 20",
                    )?
                    .query_map([&since_day], |r| {
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
        // The hour in hand, and any ended and not yet written, by their
        // most searched words: copying every word held would hold the lock
        // the engine's events wait on.
        let held: Vec<_> = {
            let hour = self.hour.lock();
            let ended = self.ended.lock();
            std::iter::once(&*hour)
                .chain(ended.iter())
                .map(|h| ((h.searches, h.matched, h.distributed), h.most(2000)))
                .collect()
        };
        for ((searches, matched, distributed), top) in held {
            totals.0 += searches;
            totals.1 += matched;
            totals.2 += distributed;
            for (term, n, m) in top {
                let e = terms.entry(term).or_default();
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
        a.flush(Utc::now(), false).await.unwrap();
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
        let kept = a.log(Some("search".into()), 10).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].distributed, "kept with the route it came by");
    }

    #[tokio::test]
    async fn visitors_and_folders_are_counted_by_day_as_rows_are_written() {
        let db = Db::memory().await;
        let a = Activity::new(db.clone());
        let (yesterday, today) = (
            stamp(Utc::now() - chrono::Duration::days(1)),
            stamp(Utc::now()),
        );
        db.run(move |c| {
            let event = "INSERT INTO share_events (at, kind, username, detail, distributed) VALUES (?1, ?2, ?3, ?4, 0)";
            c.execute(event, params![yesterday, "folder", "Ann", "Music\\BoC\\Geogaddi"])?;
            c.execute(event, params![today, "browse", "ann", None::<String>])?;
            c.execute(event, params![yesterday, "search", "Bob", "geogaddi"])?;
            c.execute(event, params![today, "search", "Bob", "geogaddi"])?;
            let upload = "INSERT INTO uploads (username, filename, size, bytes, state, finished_at) \
                          VALUES (?1, ?2, 1, 1, ?3, ?4)";
            c.execute(upload, params!["Cat", "Music\\BoC\\Geogaddi\\01.flac", "completed", today])?;
            c.execute(upload, params!["Cat", "loose.flac", "completed", today])?;
            c.execute(upload, params!["Cat", "Music\\BoC\\Geogaddi\\02.flac", "failed", today])?;
            Ok(())
        })
        .await
        .unwrap();
        let trends = a.trends(7).await.unwrap();
        assert_eq!(
            trends
                .visitors
                .iter()
                .map(|v| (v.username.as_str(), v.days, v.events))
                .collect::<Vec<_>>(),
            [("ann", 2, 2)],
            "one person whatever the case, and matched searches are not visits"
        );
        let folders: Vec<_> = trends
            .folders
            .iter()
            .map(|f| (f.folder.as_str(), f.opens, f.downloads))
            .collect();
        assert_eq!(
            folders,
            [("Music\\BoC\\Geogaddi", 1, 1), ("loose.flac", 0, 1)],
            "opens and completed uploads, by the file's folder"
        );
    }
}
