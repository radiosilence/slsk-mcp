//! Rooms, private messages and buddies.
//!
//! The open conversation lives in a hidden form on the page, not in a URL
//! or a signal, since room and user names are peer-chosen. The tab polls
//! with that form while it is showing. Sending goes through the same
//! preview-then-confirm tokens the API uses, with the Send button (after a
//! fixed-text confirm) as the confirmation.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, many, one, patch, signals};
use crate::App;
use crate::social::RoomLine;

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/chat", get(view))
        .route("/chat/open", post(open))
        .route("/chat/send", post(send))
        .route("/chat/join", post(join))
        .route("/chat/leave", post(leave))
        .route("/chat/buddy/add", post(add_buddy))
        .route("/chat/buddy/remove", post(remove_buddy))
}

/// The Chat tab's badge: private messages not yet read.
pub(super) async fn count_html(app: &App) -> String {
    let n = app.social.unread().await.unwrap_or(0);
    let text = if n > 0 { n.to_string() } else { String::new() };
    format!(r#"<span id="chat-count" class="count">{text}</span>"#)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Room,
    Private,
}

/// An open conversation.
struct Thread {
    kind: Kind,
    name: String,
}

impl Thread {
    /// Its address, as the thread form names it.
    fn url(&self) -> String {
        let kind = match self.kind {
            Kind::Room => "room",
            Kind::Private => "pm",
        };
        super::place_url("/chat", &[("kind", kind), ("name", &self.name)])
    }

    fn from_fields(form: &[(String, String)]) -> Option<Self> {
        let name = super::field(form, "name")?.to_string();
        let kind = match super::field(form, "kind")? {
            "room" => Kind::Room,
            "pm" => Kind::Private,
            _ => return None,
        };
        Some(Self { kind, name })
    }
}

struct Conversation {
    username: String,
    last: String,
    ours: bool,
    at: chrono::DateTime<chrono::Utc>,
    unread: i64,
}

struct Buddy {
    username: String,
    note: String,
    status: &'static str,
}

#[derive(Template)]
#[template(path = "chat_side.html")]
struct SideView {
    rooms: Vec<(String, usize)>,
    conversations: Vec<Conversation>,
    buddies: Vec<Buddy>,
    error: Option<String>,
}

impl SideView {
    fn ago(&self, c: &Conversation) -> String {
        super::ago(c.at)
    }
}

async fn side_html(app: &App) -> String {
    let result = async {
        let mut conversations: Vec<Conversation> = app
            .social
            .conversations()
            .await?
            .into_iter()
            .map(|(username, last, ours, at, unread)| Conversation {
                username,
                last,
                ours,
                at,
                unread,
            })
            .collect();
        conversations.sort_by_key(|c| std::cmp::Reverse(c.at));
        let buddies = app
            .social
            .buddies()
            .await?
            .into_iter()
            .map(|(username, note, status, _)| {
                use slsk_engine::slsk_proto::server::UserStatus as S;
                Buddy {
                    username,
                    note,
                    status: match status {
                        Some(S::Online) => "online",
                        Some(S::Away) => "away",
                        Some(S::Offline) => "offline",
                        None => "unknown",
                    },
                }
            })
            .collect();
        anyhow::Ok((conversations, buddies))
    }
    .await;
    let rooms = app
        .social
        .rooms(0)
        .into_iter()
        .map(|(name, members, _)| (name, members.len()))
        .collect();
    let view = match result {
        Ok((conversations, buddies)) => SideView {
            rooms,
            conversations,
            buddies,
            error: None,
        },
        Err(e) => SideView {
            rooms,
            conversations: Vec::new(),
            buddies: Vec::new(),
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "chat_rooms.html")]
struct RoomListView {
    rooms: Vec<(String, u32)>,
}

fn room_list_html(app: &App) -> String {
    RoomListView {
        rooms: app.social.room_list().into_iter().take(100).collect(),
    }
    .render()
    .unwrap_or_default()
}

struct Line {
    who: String,
    body: String,
    ours: bool,
    at: chrono::DateTime<chrono::Utc>,
}

#[derive(Template)]
#[template(path = "chat_lines.html")]
struct LinesView {
    lines: Vec<Line>,
    note: Option<String>,
}

impl LinesView {
    fn at(&self, l: &Line) -> String {
        super::ago(l.at)
    }
}

/// Newest first, so the latest is under the composer without scrolling.
async fn lines_html(app: &App, t: &Thread) -> String {
    let view = match t.kind {
        Kind::Room => match app.social.rooms(200).into_iter().find(|r| r.0 == t.name) {
            Some((_, _, lines)) => LinesView {
                note: lines
                    .is_empty()
                    .then(|| "Nothing said since joining.".into()),
                lines: lines
                    .into_iter()
                    .rev()
                    .map(
                        |RoomLine {
                             at,
                             username,
                             message,
                         }| Line {
                            who: username,
                            body: message,
                            ours: false,
                            at,
                        },
                    )
                    .collect(),
            },
            None => LinesView {
                lines: Vec::new(),
                note: Some("Not in this room yet; joining can take a moment.".into()),
            },
        },
        Kind::Private => {
            // The person is looking at it, so it has been read.
            let _ = app.social.mark_read(&t.name).await;
            match app.social.messages(&t.name, 200).await {
                Ok(rows) => LinesView {
                    note: rows.is_empty().then(|| "No messages yet.".into()),
                    lines: rows
                        .into_iter()
                        .rev()
                        .map(|(body, ours, at)| Line {
                            who: if ours { "you".into() } else { t.name.clone() },
                            body,
                            ours,
                            at,
                        })
                        .collect(),
                },
                Err(e) => LinesView {
                    lines: Vec::new(),
                    note: Some(format!("{e:#}")),
                },
            }
        }
    };
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "chat_thread.html")]
struct ThreadView {
    kind: &'static str,
    name: String,
    members: Vec<String>,
    lines: String,
}

async fn thread_html(app: &App, t: Option<&Thread>) -> String {
    let view = match t {
        None => ThreadView {
            kind: "",
            name: String::new(),
            members: Vec::new(),
            lines: String::new(),
        },
        Some(t) => ThreadView {
            kind: match t.kind {
                Kind::Room => "room",
                Kind::Private => "pm",
            },
            name: t.name.clone(),
            members: match t.kind {
                Kind::Room => app
                    .social
                    .rooms(0)
                    .into_iter()
                    .find(|r| r.0 == t.name)
                    .map(|r| r.1)
                    .unwrap_or_default(),
                Kind::Private => Vec::new(),
            },
            lines: lines_html(app, t).await,
        },
    };
    view.render().unwrap_or_default()
}

#[derive(serde::Deserialize)]
struct Poll {
    #[serde(default)]
    poll: bool,
    kind: Option<String>,
    name: Option<String>,
}

/// The lists and the open conversation's lines. Opening the tab also
/// refreshes the public room list, which polling leaves alone so that it
/// stays open or closed as the person left it.
async fn view(State(s): State<UiState>, Query(q): Query<Poll>) -> Response {
    let form: Vec<(String, String)> = [("kind", q.kind), ("name", q.name)]
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_string(), v?)))
        .collect();
    let thread = Thread::from_fields(&form);
    // The lines first: drawing a private conversation marks it read, which
    // the lists and the badge then reflect. A poll refreshes the lines of
    // the open thread; otherwise the thread is drawn whole, as when the
    // address names it.
    let mut html = match &thread {
        Some(t) if q.poll => lines_html(&s.app, t).await + "\n",
        Some(t) => thread_html(&s.app, Some(t)).await + "\n",
        None => String::new(),
    };
    html.push_str(&side_html(&s.app).await);
    if !q.poll {
        html.push('\n');
        html.push_str(&room_list_html(&s.app));
    }
    html.push('\n');
    html.push_str(&count_html(&s.app).await);
    match thread.filter(|_| !q.poll) {
        Some(t) => many(vec![patch(&html), super::place("chat", &t.url())]),
        None => one(html),
    }
}

/// Open a room (`room`) or a private conversation (`username`).
async fn open(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let thread = if let Some(room) = super::field(&form, "room") {
        Thread {
            kind: Kind::Room,
            name: room.to_string(),
        }
    } else if let Some(user) = super::field(&form, "username") {
        Thread {
            kind: Kind::Private,
            name: user.trim().to_string(),
        }
    } else {
        return failed(&anyhow::anyhow!("Say who to talk to."));
    };
    opened(&s.app, &thread, None).await
}

/// The conversation drawn afresh, the lists (its unread count now zero) and
/// the composer emptied.
async fn opened(app: &App, t: &Thread, flash: Option<String>) -> Response {
    many(vec![
        patch(&format!(
            "{}\n{}\n{}\n{}",
            thread_html(app, Some(t)).await,
            side_html(app).await,
            count_html(app).await,
            flash.unwrap_or_else(|| "<div id=\"flash\"></div>".into())
        )),
        signals(r#"{"_msg":""}"#),
        super::place("chat", &t.url()),
    ])
}

async fn send(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(t) = Thread::from_fields(&form) else {
        return failed(&anyhow::anyhow!("Open a conversation first."));
    };
    let Some(message) = super::field(&form, "message").map(str::trim) else {
        return failed(&anyhow::anyhow!("Nothing to send."));
    };
    let social = &s.app.social;
    let result = match t.kind {
        Kind::Private => {
            let token = social.preview("pm", &t.name, message);
            social.send_message(&token, &t.name, message).await
        }
        Kind::Room => {
            let token = social.preview("room", &t.name, message);
            social.say(&token, &t.name, message)
        }
    };
    match result {
        Ok(()) => opened(&s.app, &t, Some(flash_ok("Sent"))).await,
        Err(e) => failed(&e),
    }
}

async fn join(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(room) = super::field(&form, "room").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say which room."));
    };
    match s.app.social.join_room(room).await {
        Ok(()) => {
            let t = Thread {
                kind: Kind::Room,
                name: room.to_string(),
            };
            opened(&s.app, &t, Some(flash_ok(&format!("Joining {room}")))).await
        }
        Err(e) => failed(&e),
    }
}

async fn leave(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(room) = super::field(&form, "room") else {
        return failed(&anyhow::anyhow!("Say which room."));
    };
    match s.app.social.leave_room(room).await {
        Ok(()) => one(format!(
            "{}\n{}\n{}",
            thread_html(&s.app, None).await,
            side_html(&s.app).await,
            flash_ok(&format!("Left {room}"))
        )),
        Err(e) => failed(&e),
    }
}

async fn add_buddy(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(user) = super::field(&form, "username").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say who to add."));
    };
    let note = super::field(&form, "note").unwrap_or_default().trim();
    match s.app.social.add_buddy(user, note).await {
        Ok(()) => many(vec![
            patch(&format!(
                "{}\n{}",
                side_html(&s.app).await,
                flash_ok(&format!("Added {user} as a buddy"))
            )),
            signals(r#"{"_buddy":"","_buddynote":""}"#),
        ]),
        Err(e) => failed(&e),
    }
}

async fn remove_buddy(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(user) = super::field(&form, "username") else {
        return failed(&anyhow::anyhow!("Say who to remove."));
    };
    match s.app.social.remove_buddy(user).await {
        Ok(()) => one(format!(
            "{}\n<div id=\"flash\"></div>",
            side_html(&s.app).await
        )),
        Err(e) => failed(&e),
    }
}
