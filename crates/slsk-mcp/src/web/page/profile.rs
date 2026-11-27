//! Profiles: the description and picture a user shows peers who ask, and
//! what the server says about them.
//!
//! Each peer answers for itself, so seeing someone's profile means
//! connecting to them, which takes a while or fails when they are offline;
//! the server's part (status, shares, speed, interests) arrives first and
//! stands on its own. The last few answers are kept, which is also what
//! their pictures are served from. This account's own profile is kept in
//! the database and set on the engine, which hands it to any peer asking.

use koan_kit_web as kit;
use std::time::Duration;

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Multipart, State};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Query;
use slsk_engine::slsk_proto::peer::UserInfo;
use slsk_engine::slsk_proto::server::{FromServer, ToServer, UserStats, UserStatus};
use slsk_engine::{Engine, Event, Profile};

use super::{Body, Page, Tab, View};
use crate::web::{Draw, UiState, failed, human};

/// The largest picture accepted for upload, before it is scaled down.
pub(in crate::web) const MAX_UPLOAD: usize = 16 << 20;

#[derive(serde::Deserialize, Default)]
pub(in crate::web) struct ProfileQuery {
    #[serde(default)]
    pub(in crate::web) username: String,
    #[serde(default)]
    pub(in crate::web) fresh: Option<String>,
}

pub(in crate::web) struct ProfilePage;

#[derive(Template)]
#[template(path = "profile/page.html")]
struct Lookup<'a> {
    username: &'a str,
    card: Option<Html>,
    /// This account's description and whether it has a picture, for the
    /// form that edits them.
    edit: Option<(String, bool)>,
}

impl Lookup<'_> {
    fn picture_input(&self) -> kit::Input<'static> {
        kit::Input::new(kit::InputKind::File, "picture").attr("accept", "image/jpeg,image/png")
    }
}

impl Page for ProfilePage {
    const TAB: Tab = Tab::Profile;
    const STREAM: &'static str = "/stream/profile";
    type Args = View<ProfileQuery>;

    async fn body(s: &UiState, View(q): View<ProfileQuery>) -> anyhow::Result<Body> {
        let username = q.username.trim();
        let Some(engine) = s.app.session.engine().cloned() else {
            return Ok(Body::of(&Lookup {
                username,
                card: None,
                edit: None,
            }));
        };
        let user = Some(username)
            .filter(|u| !u.is_empty())
            .map_or_else(|| engine.username(), str::to_string);
        let mine = user == engine.username();
        // Asked to look again: the answers kept are forgotten, and the page
        // moves to its address, so a reload does not ask again.
        if q.fresh.is_some() {
            s.peers.info.forget(&user);
            s.peers.server.forget(&user);
            return Ok(Body::redirect(crate::web::profile_href(username)));
        }
        // What the server says, and for a peer what they say, take seconds:
        // they are asked here and the stream draws them as they arrive.
        s.peers.ask_server(&engine, &user);
        if !mine {
            s.peers.ask_info(&engine, &user);
        }
        let edit = mine.then(|| {
            let profile = engine.profile();
            (profile.description, profile.picture.is_some())
        });
        Ok(Body::of(&Lookup {
            username,
            card: Some(card(s, &engine, &user).await.draw()),
            edit,
        })
        .heading(Some(username.to_string()).filter(|u| !u.is_empty())))
    }
}

/// `username`'s profile as it stands: this account's own, or a peer's from
/// what they and the server have answered so far.
pub(in crate::web) async fn card(s: &UiState, engine: &Engine, username: &str) -> Card {
    let server = s
        .peers
        .server
        .answer(username)
        .and_then(Result::ok)
        .unwrap_or_default();
    if username == engine.username() {
        return own(s, engine, server).await;
    }
    let banned = engine.is_banned(username);
    match s.peers.info.answer(username) {
        Some(Ok(info)) => peer(username, banned, &server, Some(&info), None, false),
        Some(Err(e)) => peer(
            username,
            banned,
            &server,
            None,
            Some(format!("{username} did not send their profile: {e}")),
            false,
        ),
        None => peer(
            username,
            banned,
            &server,
            None,
            Some(format!("Asking {username} for their profile…")),
            true,
        ),
    }
}

#[derive(Template)]
#[template(path = "profile/card.html")]
pub(in crate::web) struct Card {
    username: String,
    mine: bool,
    banned: bool,
    status: Option<&'static str>,
    picture: Option<String>,
    description: String,
    /// Label and value, in the order shown.
    stats: Vec<(&'static str, String)>,
    likes: Vec<String>,
    hates: Vec<String>,
    /// Why the peer's own part is missing, or that it is on its way.
    note: Option<String>,
    /// The note says the peer's own part is on its way.
    asking: bool,
}

impl Card {
    fn pairs(&self) -> kit::Pairs<'_> {
        self.stats
            .iter()
            .map(|(label, value)| (*label, value))
            .collect()
    }

    fn browse_label(&self) -> &'static str {
        if self.mine {
            "Browse your shares"
        } else {
            "Browse shares"
        }
    }

    fn refresh(&self) -> String {
        crate::web::place_url("/profile", &[("username", &self.username), ("fresh", "1")])
    }
}

/// What the server knows of a user, as far as it answered in time.
#[derive(Default, Clone)]
pub(in crate::web) struct Server {
    pub(in crate::web) status: Option<UserStatus>,
    pub(in crate::web) stats: Option<UserStats>,
    pub(in crate::web) interests: Option<(Vec<String>, Vec<String>)>,
}

/// Ask the server for a user's status, stats and interests. Its answers
/// arrive among every other server message, picked out by name.
pub(in crate::web) async fn from_server(engine: &Engine, username: &str) -> Server {
    let mut events = engine.events();
    let user = || username.to_string();
    for msg in [
        ToServer::GetUserStatus { username: user() },
        ToServer::GetUserStats { username: user() },
        ToServer::UserInterests { username: user() },
    ] {
        if engine.send(msg).is_err() {
            return Server::default();
        }
    }
    let mut out = Server::default();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while out.status.is_none() || out.stats.is_none() || out.interests.is_none() {
            match events.recv().await {
                Ok(Event::Server(msg)) => match &*msg {
                    FromServer::UserStatus {
                        username: u,
                        status,
                        ..
                    } if u == username => out.status = Some(*status),
                    FromServer::UserStats { username: u, stats } if u == username => {
                        out.stats = Some(*stats)
                    }
                    FromServer::UserInterests {
                        username: u,
                        likes,
                        hates,
                    } if u == username => out.interests = Some((likes.clone(), hates.clone())),
                    _ => {}
                },
                Ok(Event::Status(_)) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                }
                Err(_) => return,
            }
        }
    })
    .await;
    out
}

fn status_word(s: UserStatus) -> &'static str {
    match s {
        UserStatus::Online => "online",
        UserStatus::Away => "away",
        UserStatus::Offline => "offline",
    }
}

/// A picture's address, changing with the picture so a cached copy of an
/// old one is never shown.
fn picture_href(username: &str, bytes: &[u8]) -> String {
    crate::web::place_url(
        "/profile/picture",
        &[
            ("username", username),
            ("v", &crate::web::fingerprint(bytes)),
        ],
    )
}

fn shared(stats: &UserStats) -> String {
    format!("{} files in {} folders", stats.files, stats.dirs)
}

pub(in crate::web) fn peer(
    username: &str,
    banned: bool,
    server: &Server,
    info: Option<&UserInfo>,
    note: Option<String>,
    asking: bool,
) -> Card {
    let mut stats = Vec::new();
    if let Some(st) = &server.stats {
        stats.push(("Shares", shared(st)));
        stats.push((
            "Average speed",
            format!("{}/s", human(u64::from(st.avg_speed))),
        ));
    }
    match (info, &server.stats) {
        (Some(i), _) => {
            stats.push(("Uploads", i.total_uploads.to_string()));
            stats.push(("Queue", i.queue_size.to_string()));
            stats.push((
                "Upload slot",
                if i.slots_free { "free" } else { "none free" }.into(),
            ));
        }
        (None, Some(st)) => stats.push(("Uploads", st.upload_num.to_string())),
        (None, None) => {}
    }
    let (likes, hates) = server.interests.clone().unwrap_or_default();
    Card {
        username: username.to_string(),
        mine: false,
        banned,
        status: server.status.map(status_word),
        picture: info
            .and_then(|i| i.picture.as_deref())
            .filter(|p| crate::art::mime_of(p).is_some())
            .map(|p| picture_href(username, p)),
        description: info.map(|i| i.description.clone()).unwrap_or_default(),
        stats,
        likes,
        hates,
        note,
        asking,
    }
}

/// This account's profile, as peers see it, with what it has shared and
/// fetched. The totals are kept in the database, so they span restarts.
pub(in crate::web) async fn own(s: &UiState, engine: &Engine, server: Server) -> Card {
    let me = engine.username();
    let profile = engine.profile();
    let db = &s.app.db;
    let (totals, served) = tokio::join!(crate::db::totals(db), crate::db::served_user_counts(db));
    let totals = totals.unwrap_or_default();
    let total = |name: &str| {
        totals
            .iter()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, v)| *v)
    };
    let (folders, files) = engine.share_counts();
    let mut stats = vec![("Shares", format!("{files} files in {folders} folders"))];
    if let Some(st) = &server.stats {
        stats.push((
            "Average upload speed",
            format!("{}/s", human(u64::from(st.avg_speed))),
        ));
    }
    stats.extend([
        (
            "Uploaded",
            format!(
                "{} in {} files",
                human(total("uploaded_bytes").max(0) as u64),
                total("uploads_completed")
            ),
        ),
        (
            "Users served",
            served.map(|(_, _, ever)| ever).unwrap_or(0).to_string(),
        ),
        ("Downloaded", human(total("downloaded_bytes").max(0) as u64)),
    ]);
    let (likes, hates) = server.interests.unwrap_or_default();
    Card {
        picture: profile.picture.as_deref().map(|p| picture_href(&me, p)),
        username: me,
        mine: true,
        banned: false,
        status: server.status.map(status_word),
        description: profile.description,
        stats,
        likes,
        hates,
        note: None,
        asking: false,
    }
}

/// Save this account's description and picture. The picture is kept as it
/// was unless a new one is uploaded or `clear` is ticked.
pub(in crate::web) async fn save(State(s): State<UiState>, mut multipart: Multipart) -> Response {
    let Some(engine) = s.app.session.engine().cloned() else {
        return failed(&anyhow::anyhow!("Sign in to Soulseek first."));
    };
    let mut description = None;
    let mut upload = Vec::new();
    let mut clear = false;
    let read: anyhow::Result<()> = async {
        while let Some(field) = multipart.next_field().await? {
            match field.name().unwrap_or_default() {
                "description" => description = Some(field.text().await?),
                "picture" => upload = field.bytes().await?.to_vec(),
                "clear" => clear = !field.text().await?.is_empty(),
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = read {
        return failed(&e);
    }
    let current = engine.profile();
    let picture = if !upload.is_empty() {
        match tokio::task::spawn_blocking(move || crate::art::avatar(&upload)).await {
            Ok(Ok(p)) => Some(p.into()),
            Ok(Err(e)) => return failed(&e),
            Err(e) => return failed(&e.into()),
        }
    } else if clear {
        None
    } else {
        current.picture
    };
    let profile = Profile {
        description: description
            .map(|d| d.replace("\r\n", "\n").trim().to_string())
            .unwrap_or(current.description),
        picture,
    };
    if let Err(e) = crate::db::save_profile(
        &s.app.db,
        &engine.username(),
        &profile.description,
        profile.picture.as_deref(),
    )
    .await
    {
        return failed(&e.into());
    }
    engine.set_profile(profile);
    askama_datastar::events([
        card(&s, &engine, &engine.username())
            .await
            .draw()
            .patch()
            .into(),
        crate::web::flash::ok("Profile saved.").patch().into(),
    ])
}

/// A profile picture: this account's own, or a peer's from their last
/// answer.
pub(in crate::web) async fn picture(
    State(s): State<UiState>,
    Query(q): Query<ProfileQuery>,
) -> Response {
    let bytes = match s.app.session.engine() {
        Some(e) if e.username() == q.username => e.profile().picture.map(|p| p.to_vec()),
        _ => s
            .peers
            .info
            .answer(&q.username)
            .and_then(Result::ok)
            .and_then(|i| i.picture.as_ref().map(|p| p.to_vec())),
    };
    match bytes {
        Some(b) => crate::web::page::library::image_response(b, true),
        None => http::StatusCode::NOT_FOUND.into_response(),
    }
}
