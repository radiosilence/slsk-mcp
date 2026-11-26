//! The Profile page live: what the server says of the user (status, shares,
//! speed, interests), then, for a peer, what they say of themselves. Each
//! peer answers for itself, which takes a while or fails when they are
//! offline, so the server's part is drawn first and stands on its own.

use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use slsk_engine::slsk_proto::peer::UserInfo;
use tokio::task::JoinHandle;

use super::{Changed, Last, Live, Shown};
use crate::web::page::Tab;
use crate::web::page::profile::{ProfileQuery, Server, from_server, own, peer};
use crate::web::{Draw, UiState};

/// How long a peer is given to send their profile.
const PEER_WAIT: Duration = Duration::from_secs(20);

/// The largest peer picture kept and shown. Ordinary avatars are far
/// smaller; this bounds what the cache of answers holds.
const MAX_PEER_PICTURE: usize = 1 << 20;

pub(in crate::web) struct Profile {
    username: String,
    server: Option<JoinHandle<Server>>,
    asking: Option<JoinHandle<Result<Arc<UserInfo>, String>>>,
    /// The server's part, once in.
    heard: Option<Server>,
    /// The peer's own part, once in or refused.
    info: Option<Result<Arc<UserInfo>, String>>,
    card: Last,
    due: bool,
}

impl Live for Profile {
    const TAB: Tab = Tab::Profile;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let q: ProfileQuery = shown.view();
        let mut live = Self {
            username: q.username.trim().to_string(),
            server: None,
            asking: None,
            heard: None,
            info: None,
            card: Last::default(),
            due: false,
        };
        let Some(engine) = s.app.session.engine().cloned() else {
            return live;
        };
        if live.username.is_empty() {
            live.username = engine.username();
        }
        let username = live.username.clone();
        if username != engine.username() {
            live.info = s.profiles.get(&username).map(Ok);
            if live.info.is_none() {
                let (engine, cache, username) =
                    (engine.clone(), s.profiles.clone(), username.clone());
                live.asking = Some(tokio::spawn(async move {
                    match tokio::time::timeout(PEER_WAIT, engine.user_info(&username)).await {
                        Ok(Ok(mut info)) => {
                            info.picture = info.picture.filter(|p| p.len() <= MAX_PEER_PICTURE);
                            let info = Arc::new(info);
                            cache.put(&username, info.clone());
                            Ok(info)
                        }
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("no answer in 20 seconds".to_string()),
                    }
                }));
            }
        }
        live.server = Some(tokio::spawn(async move {
            from_server(&engine, &username).await
        }));
        live
    }

    async fn draw(&mut self, s: &UiState, _: &Changed) -> Vec<Event> {
        let (Some(engine), Some(server), true) = (s.app.session.engine(), &self.heard, self.due)
        else {
            return Vec::new();
        };
        self.due = false;
        let u = self.username.as_str();
        let card = if u == engine.username() {
            own(s, engine, server.clone()).await
        } else {
            let banned = engine.is_banned(u);
            match &self.info {
                Some(Ok(info)) => peer(u, banned, server, Some(info), None, false),
                Some(Err(e)) => peer(
                    u,
                    banned,
                    server,
                    None,
                    Some(format!("{u} did not send their profile: {e}")),
                    false,
                ),
                None => peer(
                    u,
                    banned,
                    server,
                    None,
                    Some(format!("Asking {u} for their profile…")),
                    true,
                ),
            }
        };
        self.card.send(card.draw()).into_iter().collect()
    }

    async fn wake(&mut self) {
        if let Some(server) = self.server.as_mut() {
            self.heard = Some(server.await.unwrap_or_default());
            self.server = None;
        } else if let Some(asking) = self.asking.as_mut() {
            self.info = Some(asking.await.unwrap_or_else(|e| Err(e.to_string())));
            self.asking = None;
        } else {
            return std::future::pending().await;
        }
        self.due = true;
    }
}
