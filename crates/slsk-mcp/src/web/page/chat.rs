//! The Chat tab: rooms, private messages and buddies. The open conversation
//! is in the address (`kind` and `name`), which every action that opens or
//! leaves one moves; the page's stream draws what is said as it is said.
//! Sending goes through the same preview-then-confirm tokens the API uses,
//! with the Send button as the confirmation.

use askama::Template;
use askama_datastar::Html;
use axum::body::Bytes;
use axum::extract::{Query, RawQuery, State};
use axum::response::Response;
use koan_kit_web as kit;

use super::{Body, Page, Tab, View, shown};
use crate::App;
use crate::social::RoomLine;
use crate::web::{Draw, UiState, chat_href, failed, field, fields, sent, signals};

pub(in crate::web) struct Chat;

/// The conversation an address names, if any.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(in crate::web) struct ChatQuery {
    kind: String,
    name: String,
}

impl ChatQuery {
    pub(in crate::web) fn thread(&self) -> Option<Thread> {
        Thread::of(&self.kind, &self.name)
    }
}

#[derive(Template)]
#[template(path = "chat/page.html")]
struct ChatPage {
    thread: Html,
    side: Html,
    rooms: Html,
}

impl Page for Chat {
    const TAB: Tab = Tab::Chat;
    const STREAM: &'static str = "/stream/chat";
    type Args = View<ChatQuery>;

    /// The thread is drawn first: drawing a private conversation marks it
    /// read, which the lists then reflect.
    async fn body(s: &UiState, View(q): View<ChatQuery>) -> anyhow::Result<Body> {
        let thread = q.thread();
        let heading = thread.as_ref().map(|t| t.name.clone());
        let thread = ThreadView::of(&s.app, thread.as_ref()).await.draw();
        Ok(Body::of(&ChatPage {
            thread,
            side: Side::of(&s.app).await.draw(),
            rooms: Rooms::matching(&s.app, "").draw(),
        })
        .heading(heading))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Room,
    Private,
}

/// An open conversation.
pub(in crate::web) struct Thread {
    kind: Kind,
    name: String,
}

impl Thread {
    fn of(kind: &str, name: &str) -> Option<Self> {
        let kind = match kind {
            "room" => Kind::Room,
            "pm" => Kind::Private,
            _ => return None,
        };
        (!name.is_empty()).then(|| Self {
            kind,
            name: name.to_string(),
        })
    }

    fn kind(&self) -> &'static str {
        match self.kind {
            Kind::Room => "room",
            Kind::Private => "pm",
        }
    }

    /// Its address.
    fn url(&self) -> String {
        chat_href(self.kind(), &self.name)
    }

    fn query(&self) -> ChatQuery {
        ChatQuery {
            kind: self.kind().into(),
            name: self.name.clone(),
        }
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

/// The rooms joined, the private conversations and the buddies.
#[derive(Template)]
#[template(path = "chat/side.html")]
pub(in crate::web) struct Side {
    rooms: Vec<(String, usize)>,
    conversations: Vec<Conversation>,
    buddies: Vec<Buddy>,
    error: Option<String>,
}

impl Side {
    pub(in crate::web) async fn of(app: &App) -> Self {
        let rooms = app
            .social
            .rooms(0)
            .into_iter()
            .map(|(name, members, _)| (name, members.len()))
            .collect();
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
        match result {
            Ok((conversations, buddies)) => Self {
                rooms,
                conversations,
                buddies,
                error: None,
            },
            Err(e) => Self {
                rooms,
                conversations: Vec::new(),
                buddies: Vec::new(),
                error: Some(format!("{e:#}")),
            },
        }
    }
    fn ago(&self, c: &Conversation) -> String {
        crate::web::ago(c.at)
    }
}

/// The most rooms drawn; the server lists thousands, and a name typed
/// narrows them.
const ROOMS: usize = 100;

/// The public rooms, most populated first.
#[derive(Template)]
#[template(path = "chat/rooms.html")]
struct Rooms {
    rooms: Vec<(String, u32)>,
    /// Rooms that match but are not drawn.
    hidden: usize,
    /// Whether the server has sent its list at all.
    listed: bool,
}

impl Rooms {
    /// Those whose names hold `filter`.
    fn matching(app: &App, filter: &str) -> Self {
        let all = app.social.room_list();
        let listed = !all.is_empty();
        let filter = filter.trim().to_lowercase();
        let mut rooms: Vec<_> = all
            .into_iter()
            .filter(|(name, _)| filter.is_empty() || name.to_lowercase().contains(&filter))
            .collect();
        let hidden = rooms.len().saturating_sub(ROOMS);
        rooms.truncate(ROOMS);
        Self {
            rooms,
            hidden,
            listed,
        }
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(in crate::web) struct RoomsQuery {
    q: String,
}

/// The room list narrowed to names holding what was typed.
pub(in crate::web) async fn rooms(
    State(s): State<UiState>,
    Query(q): Query<RoomsQuery>,
) -> Response {
    askama_datastar::events([Rooms::matching(&s.app, &q.q).draw().patch().into()])
}

struct Line {
    who: String,
    body: String,
    ours: bool,
    at: chrono::DateTime<chrono::Utc>,
}

/// The most lines drawn of a conversation.
const LINES: usize = 200;

/// What has been said in a conversation, newest first, so the latest is
/// under the composer without scrolling.
#[derive(Template)]
#[template(path = "chat/lines.html")]
pub(in crate::web) struct Lines {
    lines: Vec<Line>,
    note: Option<String>,
}

impl Lines {
    /// Drawing a private conversation marks it read: the person is looking at it.
    pub(in crate::web) async fn of(app: &App, t: &Thread) -> Self {
        match t.kind {
            Kind::Room => match app.social.rooms(LINES).into_iter().find(|r| r.0 == t.name) {
                Some((_, _, lines)) => Self {
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
                None => Self {
                    lines: Vec::new(),
                    note: Some("Not in this room yet; joining can take a moment.".into()),
                },
            },
            Kind::Private => {
                let _ = app.social.mark_read(&t.name).await;
                match app.social.messages(&t.name, LINES as i64).await {
                    Ok(rows) => Self {
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
                    Err(e) => Self {
                        lines: Vec::new(),
                        note: Some(format!("{e:#}")),
                    },
                }
            }
        }
    }
    fn at(&self, l: &Line) -> String {
        crate::web::ago(l.at)
    }
}

/// The open conversation: who it is with, what can be done, the composer
/// and what has been said.
#[derive(Template)]
#[template(path = "chat/thread.html")]
struct ThreadView {
    kind: &'static str,
    name: String,
    members: Vec<String>,
    lines: Option<Html>,
}

impl ThreadView {
    async fn of(app: &App, t: Option<&Thread>) -> Self {
        let Some(t) = t else {
            return Self {
                kind: "",
                name: String::new(),
                members: Vec::new(),
                lines: None,
            };
        };
        Self {
            kind: t.kind(),
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
            lines: Some(Lines::of(app, t).await.draw()),
        }
    }
}

/// The page moved to `t`, with `extra` after: an action that opens a
/// conversation answers with the page it leads to.
async fn opened(s: &UiState, t: Option<&Thread>, extra: Option<kit::Flash<'_>>) -> Response {
    let (url, q) = match t {
        Some(t) => (t.url(), t.query()),
        None => ("/chat".to_string(), ChatQuery::default()),
    };
    let mut events = match shown::<Chat>(s, View(q), &url).await {
        Ok(events) => events,
        Err(e) => return failed(&e),
    };
    events.extend(extra.map(|f| f.patch().into()));
    events.push(signals(serde_json::json!({ "_msg": "" })));
    askama_datastar::events(events)
}

/// Open a room (`room`) or a private conversation (`username`).
pub(in crate::web) async fn open(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let thread = if let Some(room) = field(&form, "room") {
        Thread::of("room", room)
    } else if let Some(user) = field(&form, "username") {
        Thread::of("pm", user.trim())
    } else {
        None
    };
    match thread {
        Some(t) => opened(&s, Some(&t), None).await,
        None => failed(&anyhow::anyhow!("Say who to talk to.")),
    }
}

/// A line sent; the composer is emptied and the stream draws it.
pub(in crate::web) async fn send(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(t) = Thread::of(
        field(&form, "kind").unwrap_or_default(),
        field(&form, "name").unwrap_or_default(),
    ) else {
        return failed(&anyhow::anyhow!("Open a conversation first."));
    };
    let Some(message) = field(&form, "message").map(str::trim) else {
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
        Ok(()) => askama_datastar::events([
            crate::web::flash::ok("Sent").patch().into(),
            signals(serde_json::json!({ "_msg": "" })),
        ]),
        Err(e) => failed(&e),
    }
}

pub(in crate::web) async fn join(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(room) = field(&form, "room").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say which room."));
    };
    if let Err(e) = s.app.social.join_room(room).await {
        return failed(&e);
    }
    let t = Thread::of("room", room);
    opened(
        &s,
        t.as_ref(),
        Some(crate::web::flash::ok(format!("Joining {room}"))),
    )
    .await
}

pub(in crate::web) async fn leave(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(room) = field(&form, "room") else {
        return failed(&anyhow::anyhow!("Say which room."));
    };
    match s.app.social.leave_room(room).await {
        Ok(()) => {
            opened(
                &s,
                None,
                Some(crate::web::flash::ok(format!("Left {room}"))),
            )
            .await
        }
        Err(e) => failed(&e),
    }
}

/// A buddy added; the form is emptied and the stream draws the list.
pub(in crate::web) async fn add_buddy(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(user) = field(&form, "username").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say who to add."));
    };
    let note = field(&form, "note").unwrap_or_default().trim();
    match s.app.social.add_buddy(user, note).await {
        Ok(()) => askama_datastar::events([
            crate::web::flash::ok(format!("Added {user} as a buddy"))
                .patch()
                .into(),
            signals(serde_json::json!({ "_buddy": "", "_buddynote": "" })),
        ]),
        Err(e) => failed(&e),
    }
}

pub(in crate::web) async fn remove_buddy(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(user) = field(&form, "username") else {
        return failed(&anyhow::anyhow!("Say who to remove."));
    };
    match s.app.social.remove_buddy(user).await {
        Ok(()) => askama_datastar::events([crate::web::flash::region().patch().into()]),
        Err(e) => failed(&e),
    }
}
