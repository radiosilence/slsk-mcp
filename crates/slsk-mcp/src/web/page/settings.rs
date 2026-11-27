//! The Settings tab: the shares, upload slots and speed limits, interests,
//! and which services matching uses.

use askama::Template;
use askama_datastar::Html;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use koan_kit_web as kit;

use super::{Body, Page, Tab};
use crate::App;
use crate::web::layout::Status;
use crate::web::{Draw, UiState, failed, field, fields, human, sent, signals};

pub(in crate::web) struct Settings;

#[derive(Template)]
#[template(path = "settings/page.html")]
struct SettingsPage {
    panel: Html,
}

impl Page for Settings {
    const TAB: Tab = Tab::Settings;
    const STREAM: &'static str = "/stream/settings";
    type Args = ();

    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&SettingsPage {
            panel: Panel::of(&s.app).await.draw(),
        }))
    }
}

#[derive(Template)]
#[template(path = "settings/panel.html")]
struct Panel {
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

impl Panel {
    async fn of(app: &App) -> Self {
        let mut view = Self {
            connected: false,
            slots: 0,
            up_kb: 0,
            down_kb: 0,
            folders: 0,
            files: 0,
            interests: Vec::new(),
            discogs: app.jobs.discogs(),
            acoustid: app.jobs.acoustid(),
        };
        if let Some(e) = app.session.engine() {
            let (up, down) = e.limits();
            (view.folders, view.files) = e.share_counts();
            view.connected = true;
            view.slots = e.upload_slots();
            view.up_kb = up / 1024;
            view.down_kb = down / 1024;
            view.interests = app.social.interests().await.unwrap_or_default();
        }
        view
    }

    fn rate(&self, kb: &u64) -> String {
        if *kb == 0 {
            "unlimited".into()
        } else {
            format!("{}/s", human(kb * 1024))
        }
    }
}

/// The panel as it now stands, and what was done.
async fn saved(app: &App, msg: &str) -> Response {
    askama_datastar::events([
        Panel::of(app).await.draw().patch().into(),
        crate::web::flash::ok(msg).patch().into(),
    ])
}

/// A whole number from a form field, or a message saying which field.
fn number(form: &[(String, String)], name: &str, what: &str) -> anyhow::Result<u64> {
    field(form, name)
        .unwrap_or("0")
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{what} must be a whole number"))
}

pub(in crate::web) async fn slots(State(s): State<UiState>, body: Bytes) -> Response {
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

pub(in crate::web) async fn limits(State(s): State<UiState>, body: Bytes) -> Response {
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

pub(in crate::web) async fn rescan(State(s): State<UiState>) -> Response {
    match s.app.session.require() {
        Ok(e) => {
            let e = e.clone();
            tokio::spawn(async move { e.rescan().await });
            askama_datastar::events([
                Status::of(&s.app).draw().patch().into(),
                crate::web::flash::ok(
                    "Rescanning shares; the count above updates when it finishes",
                )
                .patch()
                .into(),
            ])
        }
        Err(e) => failed(&e),
    }
}

/// Like, dislike or forget an interest: `liked` is "true", "false" or
/// absent.
pub(in crate::web) async fn interest(
    State(s): State<UiState>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let form = sent(query.as_deref(), &body);
    let Some(item) = field(&form, "item").map(str::trim) else {
        return failed(&anyhow::anyhow!("Say what the interest is."));
    };
    let liked = match field(&form, "liked") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    match s.app.social.set_interest(item, liked).await {
        Ok(()) => askama_datastar::events([
            Panel::of(&s.app).await.draw().patch().into(),
            crate::web::flash::region().patch().into(),
            signals(serde_json::json!({ "_interest": "" })),
        ]),
        Err(e) => failed(&e),
    }
}
