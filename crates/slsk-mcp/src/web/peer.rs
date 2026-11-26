//! What the UI asks of peers and the server about them, and the Ban button
//! every page naming a peer shows: search results, uploads, profiles, shares
//! and job cards.

use std::sync::Arc;
use std::time::Duration;

use askama_datastar::ElementPatchMode;
use axum::extract::{Query, State};
use axum::response::Response;
use koan_kit_web as kit;
use slsk_engine::slsk_proto::peer::UserInfo;
use slsk_engine::slsk_proto::server::{FromServer, ToServer, UserStatus};
use slsk_engine::{Engine, Event};

use super::asked::{Asked, Outcome};
use super::page::browse::{Info, Listing};
use super::page::profile::Server;
use super::{UiState, failed, insert, place_url};

/// A peer's whole share list and what they say of themselves.
pub(super) type Shares = (Listing, Option<Info>);

/// How long a peer is given to send their profile.
const INFO_WAIT: Duration = Duration::from_secs(20);
/// How long the server is given to say whether a user is online.
const STATUS_WAIT: Duration = Duration::from_secs(5);
/// The largest peer picture kept and shown. Ordinary avatars are far
/// smaller; this bounds what is remembered.
const MAX_PICTURE: usize = 1 << 20;

/// The questions the UI asks about other users, each asked once and its
/// answer remembered (see [`Asked`]).
#[derive(Clone)]
pub(super) struct Peers {
    /// Share lists: large, slow to send, and a burden on the peer, so kept
    /// ten minutes and only the last few.
    pub(super) shares: Arc<Asked<Shares>>,
    /// What a peer says of themselves: description, picture, slots.
    pub(super) info: Arc<Asked<Arc<UserInfo>>>,
    /// What the server says of a user: status, shares, speed, interests.
    pub(super) server: Arc<Asked<Server>>,
    /// Whether a user is online, for search results.
    pub(super) status: Arc<Asked<&'static str>>,
}

impl Default for Peers {
    fn default() -> Self {
        let minute = Duration::from_secs(60);
        Self {
            shares: Arc::new(Asked::new(10 * minute, minute, 4)),
            info: Arc::new(Asked::new(10 * minute, minute, 16)),
            server: Arc::new(Asked::new(minute, minute, 64)),
            status: Arc::new(Asked::new(5 * minute, minute, 2000)),
        }
    }
}

impl Peers {
    /// Ask `username` for their share list, with their profile beside it.
    pub(super) fn ask_shares(&self, engine: &Engine, username: &str) {
        let (engine, user) = (engine.clone(), username.to_string());
        self.shares.ask(username, async move {
            let info = tokio::time::timeout(INFO_WAIT, engine.user_info(&user));
            let (dirs, info) = tokio::join!(engine.browse(&user), info);
            let listing: Listing = Arc::new(dirs.map_err(|e| e.to_string())?);
            let info = info.ok().and_then(Result::ok).map(|i| Info {
                description: i.description,
                uploads: i.total_uploads,
                queue: i.queue_size,
                free: i.slots_free,
            });
            Ok((listing, info))
        });
    }

    /// Ask `username` what they say of themselves.
    pub(super) fn ask_info(&self, engine: &Engine, username: &str) {
        let (engine, user) = (engine.clone(), username.to_string());
        self.info.ask(username, async move {
            match tokio::time::timeout(INFO_WAIT, engine.user_info(&user)).await {
                Ok(Ok(mut info)) => {
                    info.picture = info.picture.filter(|p| p.len() <= MAX_PICTURE);
                    Ok(Arc::new(info))
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("no answer in {} seconds", INFO_WAIT.as_secs())),
            }
        });
    }

    /// Ask the server what it knows of `username`.
    pub(super) fn ask_server(&self, engine: &Engine, username: &str) {
        let (engine, user) = (engine.clone(), username.to_string());
        self.server.ask(username, async move {
            Ok(super::page::profile::from_server(&engine, &user).await)
        });
    }

    /// Ask the server whether `username` is online.
    pub(super) fn ask_status(&self, engine: &Engine, username: &str) {
        let (engine, user) = (engine.clone(), username.to_string());
        self.status
            .ask(username, async move { status(&engine, &user).await });
    }
}

/// Whether `username` is online, as the server answers among every other
/// message it sends.
async fn status(engine: &Engine, username: &str) -> Outcome<&'static str> {
    let mut events = engine.events();
    engine
        .send(ToServer::GetUserStatus {
            username: username.to_string(),
        })
        .map_err(|e| e.to_string())?;
    let wait = async {
        loop {
            match events.recv().await {
                Ok(Event::Server(msg)) => {
                    if let FromServer::UserStatus {
                        username: u,
                        status,
                        ..
                    } = &*msg
                        && u == username
                    {
                        return Ok(match status {
                            UserStatus::Online => "online",
                            UserStatus::Away => "away",
                            UserStatus::Offline => "offline",
                        });
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return Err("the connection closed".to_string()),
            }
        }
    };
    tokio::time::timeout(STATUS_WAIT, wait)
        .await
        .unwrap_or_else(|_| Err("no answer".to_string()))
}

/// A peer an action names in its address, since a button posts the page's
/// signals rather than a form.
#[derive(serde::Deserialize)]
pub(super) struct PeerQuery {
    pub(super) username: String,
}

/// The key a peer's Ban button carries, so one press redraws every copy of
/// it on the page whatever characters the name holds.
fn key(username: &str) -> String {
    username.bytes().map(|b| format!("{b:02x}")).collect()
}

/// A Ban button for a peer, or Unban once banned. Banning asks first: it
/// refuses the peer everything and stops them being downloaded from.
pub(super) fn ban_button(username: &str, banned: bool) -> kit::Button<'static> {
    let (button, action) = if banned {
        (kit::Button::quiet("Unban"), "/peer/unban")
    } else {
        (
            kit::Button::quiet("Ban")
                .bad()
                .confirm("Ban this user?", "Ban")
                .detail("They get nothing more from you, and nothing is downloaded from them."),
            "/peer/ban",
        )
    };
    button
        .post(place_url(action, &[("username", username)]))
        .data("peer-ban", key(username))
}

pub(super) async fn ban(State(s): State<UiState>, Query(p): Query<PeerQuery>) -> Response {
    set(&s, &p.username, true).await
}

pub(super) async fn unban(State(s): State<UiState>, Query(p): Query<PeerQuery>) -> Response {
    set(&s, &p.username, false).await
}

/// Every copy of the peer's button on the page swapped for its other state.
/// They are found by the key they carry rather than an id, since one peer's
/// button can appear many times.
async fn set(s: &UiState, username: &str, banned: bool) -> Response {
    if let Err(e) = crate::graphql::set_ban(&s.app, username, banned).await {
        return failed(&anyhow::anyhow!("{}", e.message));
    }
    let said = if banned {
        format!("Banned {username}")
    } else {
        format!("Unbanned {username}")
    };
    askama_datastar::events([
        insert(
            &format!("[data-peer-ban=\"{}\"]", key(username)),
            ElementPatchMode::Replace,
            &ban_button(username, banned).html(),
        ),
        kit::Flash::ok(&said).patch().into(),
        kit::Confirm::close().into(),
    ])
}
