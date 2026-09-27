//! Bans, the engine's settings and interests, and triage of why albums did
//! not land.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{get, post};

use super::{UiState, failed, fields, flash_ok, human, many, one, patch, signals, status_html};
use crate::App;
use crate::graphql::TriageCause;

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/bans", get(bans))
        .route("/bans/add", post(ban))
        .route("/bans/remove", post(unban))
        .route("/settings", get(settings))
        .route("/settings/slots", post(slots))
        .route("/settings/limits", post(limits))
        .route("/settings/rescan", post(rescan))
        .route("/settings/interest", post(interest))
        .route("/triage", get(triage))
}

// --- Bans ------------------------------------------------------------------

#[derive(Template)]
#[template(path = "bans.html")]
struct BansView {
    bans: Vec<String>,
    error: Option<String>,
}

async fn bans_html(app: &App) -> String {
    let result = async {
        let engine = app.session.require()?;
        anyhow::Ok(crate::db::bans(&app.db, &engine.username()).await?)
    }
    .await;
    match result {
        Ok(bans) => BansView { bans, error: None },
        Err(e) => BansView {
            bans: Vec::new(),
            error: Some(format!("{e:#}")),
        },
    }
    .render()
    .unwrap_or_default()
}

async fn bans(State(s): State<UiState>) -> Response {
    one(bans_html(&s.app).await)
}

async fn set_ban(s: &UiState, body: &[u8], banned: bool) -> Response {
    let form = fields(body);
    let Some(user) = super::field(&form, "username").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say who."));
    };
    match crate::graphql::set_ban(&s.app, user, banned).await {
        Ok(_) => many(vec![
            patch(&format!(
                "{}\n{}",
                bans_html(&s.app).await,
                flash_ok(&if banned {
                    format!("Banned {user}")
                } else {
                    format!("Unbanned {user}")
                })
            )),
            signals(r#"{"_banuser":""}"#),
        ]),
        Err(e) => one(super::flash_err(&e.message)),
    }
}

async fn ban(State(s): State<UiState>, body: Bytes) -> Response {
    set_ban(&s, &body, true).await
}

async fn unban(State(s): State<UiState>, body: Bytes) -> Response {
    set_ban(&s, &body, false).await
}

// --- Settings --------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsView {
    connected: bool,
    slots: usize,
    /// Kilobytes per second; 0 is unlimited.
    up_kb: u64,
    down_kb: u64,
    folders: usize,
    files: usize,
    interests: Vec<(String, bool)>,
}

impl SettingsView {
    fn rate(&self, kb: &u64) -> String {
        if *kb == 0 {
            "unlimited".into()
        } else {
            format!("{}/s", human(kb * 1024))
        }
    }
}

async fn settings_html(app: &App) -> String {
    let view = match app.session.engine() {
        Some(e) => {
            let (up, down) = e.limits();
            let (folders, files) = e.share_counts();
            SettingsView {
                connected: true,
                slots: e.upload_slots(),
                up_kb: up / 1024,
                down_kb: down / 1024,
                folders,
                files,
                interests: app.social.interests().await.unwrap_or_default(),
            }
        }
        None => SettingsView {
            connected: false,
            slots: 0,
            up_kb: 0,
            down_kb: 0,
            folders: 0,
            files: 0,
            interests: Vec::new(),
        },
    };
    view.render().unwrap_or_default()
}

async fn settings(State(s): State<UiState>) -> Response {
    one(settings_html(&s.app).await)
}

async fn saved(app: &App, msg: &str) -> Response {
    one(format!("{}\n{}", settings_html(app).await, flash_ok(msg)))
}

/// A whole number from a form field, or a message saying which field.
fn number(form: &[(String, String)], name: &str, what: &str) -> anyhow::Result<u64> {
    super::field(form, name)
        .unwrap_or("0")
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{what} must be a whole number"))
}

async fn slots(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let result = async {
        let n = number(&form, "slots", "Upload slots")?.clamp(1, 100) as usize;
        s.app.session.require()?.set_upload_slots(n);
        anyhow::Ok(n)
    }
    .await;
    match result {
        Ok(n) => saved(&s.app, &format!("{n} upload slots")).await,
        Err(e) => failed(&e),
    }
}

async fn limits(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let result = async {
        let up = number(&form, "up", "The upload limit")?;
        let down = number(&form, "down", "The download limit")?;
        s.app
            .session
            .require()?
            .set_limits(up.saturating_mul(1024), down.saturating_mul(1024));
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => saved(&s.app, "Speed limits set").await,
        Err(e) => failed(&e),
    }
}

async fn rescan(State(s): State<UiState>) -> Response {
    match s.app.session.require() {
        Ok(e) => {
            let e = e.clone();
            tokio::spawn(async move { e.rescan().await });
            one(format!(
                "{}\n{}",
                status_html(&s.app).await,
                flash_ok("Rescanning shares; the count above updates when it finishes")
            ))
        }
        Err(e) => failed(&e),
    }
}

/// Like, dislike or forget an interest: `liked` is "true", "false" or
/// absent.
async fn interest(State(s): State<UiState>, body: Bytes) -> Response {
    let form = fields(&body);
    let Some(item) = super::field(&form, "item").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say what the interest is."));
    };
    let liked = match super::field(&form, "liked") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    match s.app.social.set_interest(item, liked).await {
        Ok(()) => many(vec![
            patch(&format!(
                "{}\n<div id=\"flash\"></div>",
                settings_html(&s.app).await
            )),
            signals(r#"{"_interest":""}"#),
        ]),
        Err(e) => failed(&e),
    }
}

// --- Triage ----------------------------------------------------------------

#[derive(Template)]
#[template(path = "triage.html")]
struct TriageView {
    days: i64,
    causes: Vec<TriageCause>,
    error: Option<String>,
}

impl TriageView {
    fn label(&self, cause: &str) -> &'static str {
        match cause {
            "peer_failed" => "The peer failed every file",
            "stalled_peer" => "The peer stalled",
            "corrupt_copy" => "Files that are not valid audio",
            "no_audio" => "No audio in the folder",
            "requested" => "Another copy was asked for",
            "untagged" => "Tags too poor to import as-is",
            "no_candidates" => "MusicBrainz had nothing",
            "incomplete" => "Tracks missing against the release",
            "extra_files" => "Files the release does not have",
            "weak_match" => "Match too weak to apply unasked",
            "lossy_source" => "Lossy audio",
            "upsampled" => "Upsampled audio",
            "mb_unavailable" => "MusicBrainz unavailable",
            "import_error" => "Import error",
            _ => "Other",
        }
    }
    fn ago(&self, at: &chrono::DateTime<chrono::Utc>) -> String {
        super::ago(*at)
    }
}

#[derive(serde::Deserialize)]
struct Days {
    days: Option<i64>,
}

async fn triage(State(s): State<UiState>, Query(q): Query<Days>) -> Response {
    let days = q.days.unwrap_or(7).clamp(1, 365);
    let view = match crate::graphql::triage(&s.app, days, 5).await {
        Ok(causes) => TriageView {
            days,
            causes,
            error: None,
        },
        Err(e) => TriageView {
            days,
            causes: Vec::new(),
            error: Some(e.message),
        },
    };
    one(view.render().unwrap_or_default())
}
