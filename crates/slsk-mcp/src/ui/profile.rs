//! Profiles: the description and picture a user shows peers who ask, and
//! what the server says about them.
//!
//! Each peer answers for itself, so seeing someone's profile means
//! connecting to them, which takes a while or fails when they are offline;
//! the server's part (status, shares, speed, interests) arrives first and
//! stands on its own. The last few answers are kept, which is also what
//! their pictures are served from. This account's own profile is kept in
//! the database and set on the engine, which hands it to any peer asking.

use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use axum::Router;
use axum::extract::{Multipart, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::Query;
use slsk_engine::slsk_proto::peer::UserInfo;
use slsk_engine::slsk_proto::server::{FromServer, ToServer, UserStats, UserStatus};
use slsk_engine::{Engine, Event, Profile};

use super::{UiState, failed, flash_ok, human, many, patch, sse};

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route(
            "/profile",
            get(profile)
                .post(save)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD)),
        )
        .route("/profile/picture", get(picture))
}

/// The largest picture accepted for upload, before it is scaled down.
const MAX_UPLOAD: usize = 16 << 20;
pub(super) const KEEP: usize = 16;
const FRESH: Duration = Duration::from_secs(600);

/// Peers' answers, newest last.
#[derive(Clone, Default)]
pub(super) struct Cache(Arc<parking_lot::Mutex<Vec<Answer>>>);

type Answer = (String, Instant, Arc<UserInfo>);

impl Cache {
    pub(super) fn get(&self, username: &str) -> Option<Arc<UserInfo>> {
        self.0
            .lock()
            .iter()
            .find(|(u, at, _)| u == username && at.elapsed() < FRESH)
            .map(|(_, _, i)| i.clone())
    }

    pub(super) fn put(&self, username: &str, info: Arc<UserInfo>) {
        let mut c = self.0.lock();
        c.retain(|(u, at, _)| u != username && at.elapsed() < FRESH);
        if c.len() >= KEEP {
            c.remove(0);
        }
        c.push((username.to_string(), Instant::now(), info));
    }
}

#[derive(serde::Deserialize, Default)]
pub(super) struct ProfileQuery {
    #[serde(default)]
    pub(super) username: String,
    #[serde(default)]
    fresh: Option<String>,
}

#[derive(Template)]
#[template(path = "pages/profile.html")]
struct Page<'a> {
    username: &'a str,
    signed_in: bool,
}

pub(super) fn page(s: &UiState, username: &str) -> String {
    Page {
        username: username.trim(),
        signed_in: s.app.session.engine().is_some(),
    }
    .render()
    .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "profile.html")]
struct View {
    username: String,
    mine: bool,
    status: Option<&'static str>,
    picture: Option<String>,
    description: String,
    /// Label and value, in the order shown.
    stats: Vec<(&'static str, String)>,
    likes: Vec<String>,
    hates: Vec<String>,
    /// Why the peer's own part is missing, or that it is on its way.
    note: Option<String>,
}

impl View {
    fn refresh(&self) -> String {
        super::place_url("/profile", &[("username", &self.username), ("fresh", "1")])
    }
}

/// What the server knows of a user, as far as it answered in time.
#[derive(Default)]
pub(super) struct Server {
    pub(super) status: Option<UserStatus>,
    pub(super) stats: Option<UserStats>,
    pub(super) interests: Option<(Vec<String>, Vec<String>)>,
}

/// Ask the server for a user's status, stats and interests. Its answers
/// arrive among every other server message, picked out by name.
async fn from_server(engine: &Engine, username: &str) -> Server {
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
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    super::place_url(
        "/profile/picture",
        &[("username", username), ("v", &format!("{hash:016x}"))],
    )
}

fn shared(stats: &UserStats) -> String {
    format!("{} files in {} folders", stats.files, stats.dirs)
}

/// `/profile?username=…`, fetched by the page: this account's own profile
/// at once, a peer's in two steps as its parts arrive.
async fn profile(State(s): State<UiState>, Query(q): Query<ProfileQuery>) -> Response {
    let Some(engine) = s.app.session.engine().cloned() else {
        return failed(&anyhow::anyhow!("Sign in to Soulseek first."));
    };
    let username = q.username.trim().to_string();
    if username.is_empty() || username == engine.username() {
        return many(vec![patch(&own(&s, &engine).await)]);
    }
    let cached = s.profiles.get(&username).filter(|_| q.fresh.is_none());
    let cache = s.profiles.clone();
    let asking = {
        let (engine, username) = (engine.clone(), username.clone());
        tokio::spawn(async move {
            if let Some(info) = cached {
                return Ok(info);
            }
            match tokio::time::timeout(Duration::from_secs(20), engine.user_info(&username)).await {
                Ok(Ok(info)) => {
                    let info = Arc::new(info);
                    cache.put(&username, info.clone());
                    Ok(info)
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("no answer in 20 seconds".to_string()),
            }
        })
    };
    sse(async_stream::stream! {
        let server = from_server(&engine, &username).await;
        if !asking.is_finished() {
            let note = format!("Asking {username} for their profile…");
            yield Ok(patch(&peer(&username, &server, None, Some(note))));
        }
        let html = match asking.await.map_err(|e| e.to_string()).and_then(|r| r) {
            Ok(info) => peer(&username, &server, Some(&info), None),
            Err(e) => {
                let note = format!("{username} did not send their profile: {e}");
                peer(&username, &server, None, Some(note))
            }
        };
        yield Ok(patch(&html));
    })
    .into_response()
}

pub(super) fn peer(
    username: &str,
    server: &Server,
    info: Option<&UserInfo>,
    note: Option<String>,
) -> String {
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
    View {
        username: username.to_string(),
        mine: false,
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
    }
    .render()
    .unwrap_or_default()
}

/// This account's profile, as peers see it, with what it has shared and
/// fetched. The totals are kept in the database, so they span restarts.
async fn own(s: &UiState, engine: &Engine) -> String {
    let me = engine.username();
    let profile = engine.profile();
    let db = &s.app.db;
    let (server, totals, served) = tokio::join!(
        from_server(engine, &me),
        crate::db::totals(db),
        crate::db::served_user_counts(db),
    );
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
    View {
        picture: profile.picture.as_deref().map(|p| picture_href(&me, p)),
        username: me,
        mine: true,
        status: server.status.map(status_word),
        description: profile.description,
        stats,
        likes,
        hates,
        note: None,
    }
    .render()
    .unwrap_or_default()
}

/// Save this account's description and picture. The picture is kept as it
/// was unless a new one is uploaded or `clear` is ticked.
async fn save(State(s): State<UiState>, mut multipart: Multipart) -> Response {
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
    many(vec![
        patch(&own(&s, &engine).await),
        patch(&flash_ok("Profile saved.")),
    ])
}

/// A profile picture: this account's own, or a peer's from their last
/// answer.
async fn picture(State(s): State<UiState>, Query(q): Query<ProfileQuery>) -> Response {
    let bytes = match s.app.session.engine() {
        Some(e) if e.username() == q.username => e.profile().picture.map(|p| p.to_vec()),
        _ => s
            .profiles
            .get(&q.username)
            .and_then(|i| i.picture.as_ref().map(|p| p.to_vec())),
    };
    match bytes {
        Some(b) => super::library::image_response(b, true),
        None => http::StatusCode::NOT_FOUND.into_response(),
    }
}
