//! The Activity tab: what other users do with our shares, as it happens and
//! over the last week.

use askama::Template;
use askama_datastar::Html;
use koan_kit_web as kit;

use super::{Body, Page, Tab};
use crate::App;
use crate::activity::{ShareEvent, Trends as Totals};
use crate::web::{Draw, UiState};

const DAYS: u32 = 7;

pub(in crate::web) struct Activity;

#[derive(Template)]
#[template(path = "activity/page.html")]
struct ActivityPage {
    feed: Html,
    trends: Html,
}

impl Page for Activity {
    const TAB: Tab = Tab::Activity;
    const STREAM: &'static str = "/stream/activity";
    type Args = ();

    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&ActivityPage {
            feed: Feed::now(&s.app).draw(),
            trends: Trends::of(&s.app).await.draw(),
        }))
    }
}

/// What other users did, newest first.
#[derive(Template)]
#[template(path = "activity/feed.html")]
pub(in crate::web) struct Feed {
    events: Vec<ShareEvent>,
    /// Searches this hour: all, answered by our shares, relayed by the
    /// distributed network.
    hour: (u64, u64, u64),
}

impl Feed {
    pub(in crate::web) fn now(app: &App) -> Self {
        Self {
            events: app.activity.feed(100),
            hour: app.activity.this_hour(),
        }
    }
    fn when(&self, e: &ShareEvent) -> String {
        e.at.format("%H:%M:%S").to_string()
    }
    fn what(&self, e: &ShareEvent) -> &'static str {
        match e.kind.as_str() {
            "browse" => "browsed your shares",
            "folder" => "opened",
            "search" => "searched",
            "upload" => "started downloading",
            "uploaded" => "downloaded",
            _ => "stopped downloading",
        }
    }
    fn leaf<'a>(&self, path: &'a str) -> &'a str {
        crate::web::leaf(path)
    }
}

/// The last week, summed up.
#[derive(Template)]
#[template(path = "activity/trends.html")]
pub(in crate::web) struct Trends {
    trends: Option<Totals>,
    error: Option<String>,
}

impl Trends {
    pub(in crate::web) async fn of(app: &App) -> Self {
        match app.activity.trends(DAYS).await {
            Ok(t) => Self {
                trends: Some(t),
                error: None,
            },
            Err(e) => Self {
                trends: None,
                error: Some(format!("{e:#}")),
            },
        }
    }
    fn share(&self, part: &u64, whole: &u64) -> u64 {
        (part * 100).checked_div(*whole).unwrap_or(0)
    }
    fn search_href(&self, term: &str) -> String {
        crate::web::place_url("/search", &[("q", term)])
    }
    fn ago(&self, t: &chrono::DateTime<chrono::Utc>) -> String {
        crate::web::ago(*t)
    }
}
