//! What every page shares: the status line, the nav with its badges, and the
//! live stream that keeps both current.

use askama::Template;
use askama_datastar::Html;
use axum::extract::State;
use axum::response::Response;
use koan_kit_web as kit;

use super::page::Tab;
use super::{APP_CSS, Draw, ICON_32, ICON_192, LIVE_JS, TOUCH_ICON, UiState, failed, human, one};
use crate::App;

/// The document every page is drawn in, around the page's own body.
#[derive(Template)]
#[template(path = "layout.html")]
pub(super) struct Layout {
    pub(super) title: Html,
    pub(super) status: Html,
    pub(super) nav: Html,
    /// The address of the page's live stream.
    pub(super) stream: &'static str,
    /// The page's starting signals.
    pub(super) signals: String,
    pub(super) main: Html,
    /// The server's clock as it drew the page (ms since the epoch), for
    /// `live.js` to correct a client clock that differs.
    pub(super) now: i64,
}

impl Layout {
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    fn css(&self) -> String {
        APP_CSS.url("/assets/app.css")
    }
    fn live(&self) -> String {
        LIVE_JS.url("/assets/live.js")
    }
    fn icons(&self) -> [String; 3] {
        [
            ICON_32.url("/assets/icon-32.png"),
            ICON_192.url("/assets/icon-192.png"),
            TOUCH_ICON.url("/assets/apple-touch-icon.png"),
        ]
    }
    /// The page's live stream, opened again whenever it ends or goes quiet, and afresh when a signal it answers for
    /// changes.
    fn stream(&self) -> Html {
        askama_datastar::reopen_on(self.stream, &["_url", "jobs", "history", "files"])
    }
}

/// The document's title: what the page shows, then the app.
#[derive(Template)]
#[template(path = "layout/title.html")]
pub(super) struct Title<'a> {
    name: &'a str,
}

impl<'a> Title<'a> {
    /// The page's heading when it has one, else its tab's name.
    pub(super) fn of(tab: Tab, heading: Option<&'a str>) -> Self {
        Self {
            name: heading.unwrap_or(tab.label()),
        }
    }
}

/// The page's own region, which a link or form loading in place replaces whole.
#[derive(Template)]
#[template(path = "layout/main.html")]
pub(super) struct Main {
    pub(super) body: Html,
}

/// The tabs, the one shown marked, with the badges of those that have one.
#[derive(Template)]
#[template(path = "layout/nav.html")]
pub(super) struct Nav {
    tab: Tab,
    /// Files being sent or waiting to be.
    uploads: usize,
    /// Private messages not yet read.
    unread: i64,
}

impl Nav {
    pub(super) async fn of(app: &App, tab: Tab) -> Self {
        Self {
            tab,
            uploads: app.session.engine().map_or(0, |e| e.uploads_pending()),
            unread: app.social.unread().await.unwrap_or(0),
        }
    }
    fn tabs(&self) -> &'static [Tab] {
        Tab::ALL
    }
    fn badge(&self, t: &Tab) -> Option<String> {
        let n = match t {
            Tab::Uploads => self.uploads as i64,
            Tab::Chat => self.unread,
            _ => 0,
        };
        (n > 0).then(|| n.to_string())
    }
}

#[derive(Template)]
#[template(path = "layout/status.html")]
pub(super) struct Status {
    state: &'static str,
    username: Option<String>,
    detail: Option<String>,
    files: usize,
    up: String,
    down: String,
    children: usize,
    /// The distributed network's parent, which passes searches down to us.
    parent: Option<String>,
    /// Searches this hour, and the hourly budget (0 for none).
    searches: (usize, usize),
    /// Once the budget is spent, when the next search is free (ms since the
    /// epoch).
    search_free: Option<i64>,
}

impl Status {
    pub(super) fn of(app: &App) -> Self {
        let Some(e) = app.session.engine() else {
            return Self {
                state: "no-account",
                username: None,
                detail: None,
                files: 0,
                up: String::new(),
                down: String::new(),
                children: 0,
                parent: None,
                searches: (0, 0),
                search_free: None,
            };
        };
        use slsk_engine::Status as S;
        let (state, detail) = match &*e.status().borrow() {
            S::Connecting => ("connecting", None),
            S::LoggedIn { .. } => ("connected", None),
            S::Rejected { reason } => ("rejected", Some(reason.clone())),
            S::Displaced => (
                "displaced",
                Some("another client logged in with this account".to_string()),
            ),
            S::Disconnected { error } => ("disconnected", Some(error.clone())),
        };
        let (up, down) = e.speeds();
        let (used, budget, frees) = e.search_budget();
        let (parent, _, _, children) = e.distributed();
        Self {
            state,
            username: Some(e.username()),
            detail,
            files: e.share_counts().1,
            up: human(up) + "/s",
            down: human(down) + "/s",
            children,
            parent,
            searches: (used, budget),
            search_free: frees
                .filter(|_| budget > 0 && used >= budget)
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
        }
    }
}

#[derive(serde::Deserialize)]
pub(super) struct AccountForm {
    username: String,
    password: String,
}

pub(super) async fn account(
    State(s): State<UiState>,
    axum::Form(f): axum::Form<AccountForm>,
) -> Response {
    let (u, p) = (f.username.trim(), f.password.as_str());
    if u.is_empty() || p.is_empty() {
        return failed(&anyhow::anyhow!("Give both the username and the password."));
    }
    match s.app.session.use_account(u, p).await {
        Ok(_) => {
            let _ = s.app.jobs.resume().await;
            one(Status::of(&s.app).draw())
        }
        Err(e) => failed(&e),
    }
}

pub(super) async fn reconnect(State(s): State<UiState>) -> Response {
    if let Some(e) = s.app.session.engine() {
        e.reconnect();
    }
    one(Status::of(&s.app).draw())
}
