//! Bans, the engine's settings and interests, and triage of why albums did
//! not land.

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, RawQuery, State};
use axum::response::Response;
use axum::routing::{get, post};
use koan_kit_web as kit;

use super::{
    Draw, Fragment, UiState, failed, fields, human, one, patch, sent, signals, status_html,
};
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

async fn bans_html(app: &App) -> Fragment {
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
    .draw()
}

#[derive(Template)]
#[template(path = "pages/bans.html")]
struct BansPage {
    list: Fragment,
}

pub(super) async fn bans_page(app: &App) -> Fragment {
    BansPage {
        list: bans_html(app).await,
    }
    .draw()
}

async fn bans(State(s): State<UiState>) -> Response {
    one(bans_html(&s.app).await)
}

async fn set_ban(s: &UiState, user: &str, banned: bool) -> Response {
    let user = user.trim();
    if user.is_empty() {
        return failed(&anyhow::anyhow!("Say who."));
    }
    match crate::graphql::set_ban(&s.app, user, banned).await {
        Ok(_) => askama_datastar::events(vec![
            patch(&Fragment::from_iter([
                bans_html(&s.app).await,
                kit::Flash::ok(&if banned {
                    format!("Banned {user}")
                } else {
                    format!("Unbanned {user}")
                })
                .html(),
            ])),
            signals(r#"{"_banuser":""}"#),
            kit::Confirm::close().into(),
        ]),
        Err(e) => one(kit::Flash::bad(&e.message).html()),
    }
}

/// The name typed on the bans page, posted by its form once the page's question is answered.
async fn ban(State(s): State<UiState>, body: axum::body::Bytes) -> Response {
    let form = super::fields(&body);
    set_ban(
        &s,
        super::field(&form, "username").unwrap_or_default(),
        true,
    )
    .await
}

async fn unban(State(s): State<UiState>, Query(p): Query<super::PeerQuery>) -> Response {
    set_ban(&s, &p.username, false).await
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
    discogs: bool,
    acoustid: bool,
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

async fn settings_html(app: &App) -> Fragment {
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
                discogs: app.jobs.discogs(),
                acoustid: app.jobs.acoustid(),
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
            discogs: app.jobs.discogs(),
            acoustid: app.jobs.acoustid(),
        },
    };
    view.draw()
}

#[derive(Template)]
#[template(path = "pages/settings.html")]
struct SettingsPage {
    settings: Fragment,
}

pub(super) async fn settings_page(app: &App) -> Fragment {
    SettingsPage {
        settings: settings_html(app).await,
    }
    .draw()
}

async fn settings(State(s): State<UiState>) -> Response {
    one(settings_html(&s.app).await)
}

async fn saved(app: &App, msg: &str) -> Response {
    one(Fragment::from_iter([
        settings_html(app).await,
        kit::Flash::ok(msg).html(),
    ]))
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
        let n = number(&form, "slots", "Upload slots")?.max(1) as usize;
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
            one(Fragment::from_iter([
                status_html(&s.app).await,
                kit::Flash::ok("Rescanning shares; the count above updates when it finishes")
                    .html(),
            ]))
        }
        Err(e) => failed(&e),
    }
}

/// Like, dislike or forget an interest: `liked` is "true", "false" or
/// absent.
async fn interest(State(s): State<UiState>, RawQuery(query): RawQuery, body: Bytes) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(item) = super::field(&form, "item").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say what the interest is."));
    };
    let liked = match super::field(&form, "liked") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    match s.app.social.set_interest(item, liked).await {
        Ok(()) => askama_datastar::events(vec![
            patch(&Fragment::from_iter([
                settings_html(&s.app).await,
                kit::Flash::region().html(),
            ])),
            signals(r#"{"_interest":""}"#),
        ]),
        Err(e) => failed(&e),
    }
}

// --- Triage ----------------------------------------------------------------

/// Why an outcome happened, from the fixed set of causes, as a person would say it.
pub(super) fn cause_label(cause: &str) -> &'static str {
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
        "discogs_unavailable" => "Discogs unavailable",
        "import_error" => "Import error",
        _ => "Other",
    }
}

#[derive(Template)]
#[template(path = "triage.html")]
struct TriageView {
    days: i64,
    causes: Vec<TriageCause>,
    error: Option<String>,
}

impl TriageView {
    fn label(&self, cause: &str) -> &'static str {
        cause_label(cause)
    }
    fn ago(&self, at: &chrono::DateTime<chrono::Utc>) -> String {
        super::ago(*at)
    }
}

#[derive(serde::Deserialize, Default)]
pub(super) struct Days {
    days: Option<i64>,
}

#[derive(Template)]
#[template(path = "pages/triage.html")]
struct TriagePage {
    triage: Fragment,
}

pub(super) async fn triage_page(app: &App, q: &Days) -> Fragment {
    TriagePage {
        triage: triage_html(app, q.days).await,
    }
    .draw()
}

async fn triage(State(s): State<UiState>, Query(q): Query<Days>) -> Response {
    let days = q.days.unwrap_or(7).clamp(1, 365);
    // Its periods load in place, moving the address themselves.
    one(triage_html(&s.app, Some(days)).await)
}

/// The newest events drawn for each cause; the count covers the rest.
const EXAMPLES: usize = 20;

async fn triage_html(app: &App, days: Option<i64>) -> Fragment {
    let days = days.unwrap_or(7).clamp(1, 365);
    let view = match crate::graphql::triage(app, days, EXAMPLES).await {
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
    view.draw()
}
