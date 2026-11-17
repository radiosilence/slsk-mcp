//! The Activity page: what other users do with our shares, as it happens
//! and over the last week.

use askama::Template;

use crate::App;
use crate::activity::{ShareEvent, Trends};

const DAYS: u32 = 7;

#[derive(Template)]
#[template(path = "activity_feed.html")]
struct FeedView {
    events: Vec<ShareEvent>,
    /// Searches this hour: all, answered by our shares, relayed by the
    /// distributed network.
    hour: (u64, u64, u64),
}

impl FeedView {
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
        path.rsplit('\\').next().unwrap_or(path)
    }
}

#[derive(Template)]
#[template(path = "activity_trends.html")]
struct TrendsView {
    trends: Option<Trends>,
    error: Option<String>,
}

impl TrendsView {
    fn share(&self, part: &u64, whole: &u64) -> u64 {
        (part * 100).checked_div(*whole).unwrap_or(0)
    }
    fn search_href(&self, term: &str) -> String {
        super::place_url("/search", &[("q", term)])
    }
    fn ago(&self, t: &chrono::DateTime<chrono::Utc>) -> String {
        super::ago(*t)
    }
}

pub(super) fn feed_html(app: &App) -> String {
    FeedView {
        events: app.activity.feed(100),
        hour: app.activity.this_hour(),
    }
    .render()
    .unwrap_or_default()
}

pub(super) async fn trends_html(app: &App) -> String {
    let view = match app.activity.trends(DAYS).await {
        Ok(t) => TrendsView {
            trends: Some(t),
            error: None,
        },
        Err(e) => TrendsView {
            trends: None,
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
}

#[derive(Template)]
#[template(path = "pages/activity.html")]
struct Page {
    feed: String,
    trends: String,
}

pub(super) async fn page(app: &App) -> String {
    Page {
        feed: feed_html(app),
        trends: trends_html(app).await,
    }
    .render()
    .unwrap_or_default()
}
