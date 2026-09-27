//! The social half of the client: private messages, rooms, buddies, the
//! wishlist and interests.
//!
//! The server keeps none of this across a session — rooms joined, users
//! watched and interests are forgotten on disconnect — so what should outlast
//! one is in the database and re-sent on every login. Room chatter is kept in
//! memory, a bounded tail per room: it is ephemeral on the network too.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use slsk_engine::slsk_proto::server::{FromServer, ToServer, UserStatus};
use slsk_engine::{Engine, Event, Status};
use sqlx::PgPool;
use uuid::Uuid;

use crate::folders::Filter;
use crate::jobs::Jobs;
use crate::session::Session;

const ROOM_TAIL: usize = 500;
/// How long a preview stays confirmable.
const NONCE_TTL: Duration = Duration::from_secs(900);

#[derive(Clone, async_graphql::SimpleObject)]
pub struct RoomLine {
    pub at: chrono::DateTime<chrono::Utc>,
    pub username: String,
    pub message: String,
}

#[derive(Default)]
struct Room {
    members: BTreeSet<String>,
    lines: VecDeque<RoomLine>,
}

pub struct Social {
    db: PgPool,
    session: Arc<Session>,
    jobs: Arc<Jobs>,
    rooms: RwLock<HashMap<String, Room>>,
    room_list: RwLock<Vec<(String, u32)>>,
    statuses: DashMap<String, (UserStatus, bool)>,
    /// Seconds between wishlist searches, as the server sets it.
    wishlist_interval: AtomicU64,
    nonces: Mutex<HashMap<String, (u64, Instant)>>,
}

impl Social {
    pub fn new(db: PgPool, session: Arc<Session>, jobs: Arc<Jobs>) -> Arc<Self> {
        Arc::new(Self {
            db,
            session,
            jobs,
            rooms: RwLock::new(HashMap::new()),
            room_list: RwLock::new(Vec::new()),
            statuses: DashMap::new(),
            wishlist_interval: AtomicU64::new(720),
            nonces: Mutex::new(HashMap::new()),
        })
    }

    fn engine(&self) -> Result<Engine> {
        Ok(self.session.require()?.clone())
    }

    /// Follow the engine's events for as long as the process runs, starting
    /// whenever an account first appears.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let engine = loop {
                if let Some(e) = me.session.engine() {
                    break e.clone();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            };
            if matches!(*engine.status().borrow(), Status::LoggedIn { .. }) {
                me.on_login(&engine).await;
            }
            let mut events = engine.events();
            loop {
                match events.recv().await {
                    Ok(Event::Status(Status::LoggedIn { .. })) => me.on_login(&engine).await,
                    Ok(Event::Status(_)) => {}
                    Ok(Event::Server(msg)) => me.on_server(&engine, &msg).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "social events lagged")
                    }
                    Err(_) => return,
                }
            }
        });
        let me = self.clone();
        tokio::spawn(async move { me.wishlist_loop().await });
    }

    async fn on_login(&self, engine: &Engine) {
        let account = engine.username();
        self.rooms.write().clear();
        let _ = engine.send(ToServer::RoomList);
        for room in sqlx::query_scalar::<_, String>("SELECT name FROM rooms WHERE account = $1")
            .bind(&account)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default()
        {
            let _ = engine.send(ToServer::JoinRoom {
                room,
                private: false,
            });
        }
        for username in self.buddy_names(&account).await {
            let _ = engine.send(ToServer::WatchUser { username });
        }
        for (item, liked) in sqlx::query_as::<_, (String, bool)>(
            "SELECT item, liked FROM interests WHERE account = $1",
        )
        .bind(&account)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default()
        {
            let _ = engine.send(if liked {
                ToServer::AddThingILike { item }
            } else {
                ToServer::AddThingIHate { item }
            });
        }
    }

    async fn on_server(&self, engine: &Engine, msg: &FromServer) {
        match msg {
            FromServer::MessageUser {
                username, message, ..
            } => {
                let _ = sqlx::query("INSERT INTO messages (account, peer, outgoing, body) VALUES ($1, $2, FALSE, $3)")
                    .bind(engine.username())
                    .bind(username)
                    .bind(message)
                    .execute(&self.db)
                    .await;
            }
            FromServer::SayChatroom {
                room,
                username,
                message,
            } => {
                let mut rooms = self.rooms.write();
                let r = rooms.entry(room.clone()).or_default();
                if r.lines.len() >= ROOM_TAIL {
                    r.lines.pop_front();
                }
                r.lines.push_back(RoomLine {
                    at: chrono::Utc::now(),
                    username: username.clone(),
                    message: message.clone(),
                });
            }
            FromServer::JoinRoom { room, users, .. } => {
                self.rooms.write().entry(room.clone()).or_default().members =
                    users.iter().map(|u| u.username.clone()).collect();
            }
            FromServer::LeaveRoom { room } => {
                self.rooms.write().remove(room);
            }
            FromServer::UserJoinedRoom { room, user } => {
                if let Some(r) = self.rooms.write().get_mut(room) {
                    r.members.insert(user.username.clone());
                }
            }
            FromServer::UserLeftRoom { room, username } => {
                if let Some(r) = self.rooms.write().get_mut(room) {
                    r.members.remove(username);
                }
            }
            FromServer::RoomList(list) => *self.room_list.write() = list.public.clone(),
            FromServer::UserStatus {
                username,
                status,
                privileged,
            } => {
                self.statuses
                    .insert(username.clone(), (*status, *privileged));
            }
            FromServer::WatchUser {
                username, status, ..
            } => {
                self.statuses
                    .entry(username.clone())
                    .or_insert((*status, false))
                    .0 = *status;
            }
            FromServer::WishlistInterval { seconds } => self
                .wishlist_interval
                .store(u64::from(*seconds).max(60), Ordering::Relaxed),
            _ => {}
        }
    }

    // --- Two-step sending --------------------------------------------------

    /// A token that `confirm` will accept for exactly this message, for a
    /// while. Messages go to people; an assistant shows the user what it is
    /// about to say before it says it.
    pub fn preview(&self, kind: &str, target: &str, body: &str) -> String {
        let token = Uuid::new_v4().to_string();
        let mut nonces = self.nonces.lock();
        nonces.retain(|_, (_, at)| at.elapsed() < NONCE_TTL);
        nonces.insert(
            token.clone(),
            (fingerprint(kind, target, body), Instant::now()),
        );
        token
    }

    fn confirm(&self, token: &str, kind: &str, target: &str, body: &str) -> Result<()> {
        let (print, at) = self
            .nonces
            .lock()
            .remove(token)
            .context("unknown or used confirmation token: run PREVIEW again")?;
        if at.elapsed() > NONCE_TTL {
            bail!("confirmation token expired: run PREVIEW again");
        }
        if print != fingerprint(kind, target, body) {
            bail!("the message changed between PREVIEW and CONFIRM: run PREVIEW again");
        }
        Ok(())
    }

    pub async fn send_message(&self, token: &str, username: &str, message: &str) -> Result<()> {
        self.confirm(token, "pm", username, message)?;
        let engine = self.engine()?;
        engine.send(ToServer::MessageUser {
            username: username.into(),
            message: message.into(),
        })?;
        sqlx::query("INSERT INTO messages (account, peer, outgoing, body, read) VALUES ($1, $2, TRUE, $3, TRUE)")
            .bind(engine.username())
            .bind(username)
            .bind(message)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub fn say(&self, token: &str, room: &str, message: &str) -> Result<()> {
        self.confirm(token, "room", room, message)?;
        self.engine()?.send(ToServer::SayChatroom {
            room: room.into(),
            message: message.into(),
        })?;
        Ok(())
    }

    // --- Rooms ---------------------------------------------------------------

    pub async fn join_room(&self, room: &str) -> Result<()> {
        let engine = self.engine()?;
        sqlx::query("INSERT INTO rooms (account, name) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(engine.username())
            .bind(room)
            .execute(&self.db)
            .await?;
        engine.send(ToServer::JoinRoom {
            room: room.into(),
            private: false,
        })?;
        Ok(())
    }

    pub async fn leave_room(&self, room: &str) -> Result<()> {
        let engine = self.engine()?;
        sqlx::query("DELETE FROM rooms WHERE account = $1 AND name = $2")
            .bind(engine.username())
            .bind(room)
            .execute(&self.db)
            .await?;
        engine.send(ToServer::LeaveRoom { room: room.into() })?;
        Ok(())
    }

    /// Joined rooms: name, members, and the last `tail` lines.
    pub fn rooms(&self, tail: usize) -> Vec<(String, Vec<String>, Vec<RoomLine>)> {
        let rooms = self.rooms.read();
        let mut out: Vec<_> = rooms
            .iter()
            .map(|(name, r)| {
                let skip = r.lines.len().saturating_sub(tail);
                (
                    name.clone(),
                    r.members.iter().cloned().collect(),
                    r.lines.iter().skip(skip).cloned().collect(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn room_list(&self) -> Vec<(String, u32)> {
        let mut list = self.room_list.read().clone();
        list.sort_by_key(|r| std::cmp::Reverse(r.1));
        list
    }

    // --- Buddies -------------------------------------------------------------

    async fn buddy_names(&self, account: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT username FROM buddies WHERE account = $1 ORDER BY username")
            .bind(account)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default()
    }

    pub async fn buddies(&self) -> Result<Vec<(String, String, Option<UserStatus>, bool)>> {
        let engine = self.engine()?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT username, note FROM buddies WHERE account = $1 ORDER BY username",
        )
        .bind(engine.username())
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(u, note)| {
                let s = self.statuses.get(&u).map(|s| *s);
                (u, note, s.map(|s| s.0), s.is_some_and(|s| s.1))
            })
            .collect())
    }

    pub async fn add_buddy(&self, username: &str, note: &str) -> Result<()> {
        let engine = self.engine()?;
        sqlx::query("INSERT INTO buddies (account, username, note) VALUES ($1, $2, $3) ON CONFLICT (account, username) DO UPDATE SET note = EXCLUDED.note")
            .bind(engine.username())
            .bind(username)
            .bind(note)
            .execute(&self.db)
            .await?;
        engine.send(ToServer::WatchUser {
            username: username.into(),
        })?;
        Ok(())
    }

    pub async fn remove_buddy(&self, username: &str) -> Result<()> {
        let engine = self.engine()?;
        sqlx::query("DELETE FROM buddies WHERE account = $1 AND username = $2")
            .bind(engine.username())
            .bind(username)
            .execute(&self.db)
            .await?;
        engine.send(ToServer::UnwatchUser {
            username: username.into(),
        })?;
        Ok(())
    }

    // --- Messages ------------------------------------------------------------

    /// One row per peer: the latest message and how many are unread.
    pub async fn conversations(
        &self,
    ) -> Result<Vec<(String, String, bool, chrono::DateTime<chrono::Utc>, i64)>> {
        let engine = self.engine()?;
        Ok(sqlx::query_as(
            "SELECT DISTINCT ON (peer) peer, body, outgoing, at,
                    (SELECT count(*) FROM messages u WHERE u.account = m.account AND u.peer = m.peer AND NOT u.read AND NOT u.outgoing)
             FROM messages m WHERE account = $1 ORDER BY peer, at DESC",
        )
        .bind(engine.username())
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn messages(
        &self,
        peer: &str,
        limit: i64,
    ) -> Result<Vec<(String, bool, chrono::DateTime<chrono::Utc>)>> {
        let engine = self.engine()?;
        let mut rows: Vec<(String, bool, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("SELECT body, outgoing, at FROM messages WHERE account = $1 AND peer = $2 ORDER BY at DESC LIMIT $3")
                .bind(engine.username())
                .bind(peer)
                .bind(limit)
                .fetch_all(&self.db)
                .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Private messages received and not yet read, across everyone.
    pub async fn unread(&self) -> Result<i64> {
        let engine = self.engine()?;
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM messages WHERE account = $1 AND NOT read AND NOT outgoing",
        )
        .bind(engine.username())
        .fetch_one(&self.db)
        .await?)
    }

    pub async fn mark_read(&self, peer: &str) -> Result<()> {
        let engine = self.engine()?;
        sqlx::query(
            "UPDATE messages SET read = TRUE WHERE account = $1 AND peer = $2 AND NOT read",
        )
        .bind(engine.username())
        .bind(peer)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    // --- Interests -----------------------------------------------------------

    pub async fn set_interest(&self, item: &str, liked: Option<bool>) -> Result<()> {
        let engine = self.engine()?;
        let account = engine.username();
        let previous: Option<bool> = sqlx::query_scalar(
            "DELETE FROM interests WHERE account = $1 AND item = $2 RETURNING liked",
        )
        .bind(&account)
        .bind(item)
        .fetch_optional(&self.db)
        .await?;
        match previous {
            Some(true) => engine.send(ToServer::RemoveThingILike { item: item.into() })?,
            Some(false) => engine.send(ToServer::RemoveThingIHate { item: item.into() })?,
            None => {}
        }
        if let Some(liked) = liked {
            sqlx::query("INSERT INTO interests (account, item, liked) VALUES ($1, $2, $3)")
                .bind(&account)
                .bind(item)
                .bind(liked)
                .execute(&self.db)
                .await?;
            engine.send(if liked {
                ToServer::AddThingILike { item: item.into() }
            } else {
                ToServer::AddThingIHate { item: item.into() }
            })?;
        }
        Ok(())
    }

    pub async fn interests(&self) -> Result<Vec<(String, bool)>> {
        let engine = self.engine()?;
        Ok(
            sqlx::query_as("SELECT item, liked FROM interests WHERE account = $1 ORDER BY item")
                .bind(engine.username())
                .fetch_all(&self.db)
                .await?,
        )
    }

    // --- Wishlist ------------------------------------------------------------

    pub async fn add_wish(&self, query: &str, lossless: bool, grab: bool) -> Result<Uuid> {
        let engine = self.engine()?;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO wishes (id, account, query, lossless, grab) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(engine.username())
        .bind(query)
        .bind(lossless)
        .bind(grab)
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    pub async fn remove_wish(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM wishes WHERE id = $1")
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn wishes(&self) -> Result<Vec<Wish>> {
        let engine = self.engine()?;
        Ok(sqlx::query_as("SELECT id, query, lossless, grab, searched_at, job_id FROM wishes WHERE account = $1 ORDER BY created_at")
            .bind(engine.username())
            .fetch_all(&self.db)
            .await?)
    }

    /// One wish per server-set interval, least recently searched first — the
    /// same pace the official client keeps, which is what the server expects
    /// of a wishlist.
    async fn wishlist_loop(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(
                self.wishlist_interval.load(Ordering::Relaxed),
            ))
            .await;
            if let Err(e) = self.wishlist_tick().await {
                tracing::debug!(error = %e, "wishlist search skipped");
            }
        }
    }

    async fn wishlist_tick(&self) -> Result<()> {
        let engine = self.engine()?;
        let Some(wish): Option<Wish> = sqlx::query_as(
            "SELECT id, query, lossless, grab, searched_at, job_id FROM wishes
             WHERE account = $1 AND job_id IS NULL ORDER BY searched_at NULLS FIRST LIMIT 1",
        )
        .bind(engine.username())
        .fetch_optional(&self.db)
        .await?
        else {
            return Ok(());
        };
        sqlx::query("UPDATE wishes SET searched_at = now() WHERE id = $1")
            .bind(wish.id)
            .execute(&self.db)
            .await?;
        let mut rx = engine.wishlist_search(&wish.query)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut responses = Vec::new();
        while let Ok(Some(r)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            responses.push(r);
        }
        let filter = Filter {
            lossless: wish.lossless,
            ..Default::default()
        };
        let found =
            crate::folders::relevant(crate::folders::group(&responses, &filter), &wish.query);
        tracing::info!(query = %wish.query, folders = found.len(), "wishlist search");
        if wish.grab {
            let mut found = found.into_iter();
            if let Some(best) = found.next() {
                let alternates = found.take(4).map(crate::db::Alternate::from).collect();
                let job = self
                    .jobs
                    .from_folder(&best, Some(wish.query.clone()), alternates)
                    .await?;
                sqlx::query("UPDATE wishes SET job_id = $2 WHERE id = $1")
                    .bind(wish.id)
                    .bind(job.id)
                    .execute(&self.db)
                    .await?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, sqlx::FromRow, async_graphql::SimpleObject)]
pub struct Wish {
    pub id: Uuid,
    pub query: String,
    pub lossless: bool,
    /// Start a job for the first relevant folder found.
    pub grab: bool,
    pub searched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The job it became, once found.
    pub job_id: Option<Uuid>,
}

fn fingerprint(kind: &str, target: &str, body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (kind, target, body).hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmation_matches_only_what_was_previewed_and_only_once() {
        let nonces: Mutex<HashMap<String, (u64, Instant)>> = Mutex::new(HashMap::new());
        let token = "t".to_string();
        nonces.lock().insert(
            token.clone(),
            (fingerprint("pm", "bob", "hi"), Instant::now()),
        );
        assert_ne!(
            fingerprint("pm", "bob", "hi"),
            fingerprint("pm", "bob", "hi!")
        );
        assert_ne!(
            fingerprint("pm", "bob", "hi"),
            fingerprint("room", "bob", "hi")
        );
        assert!(nonces.lock().remove(&token).is_some());
        assert!(nonces.lock().remove(&token).is_none());
    }
}
