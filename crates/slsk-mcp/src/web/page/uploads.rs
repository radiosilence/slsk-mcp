//! The Uploads tab: who is taking what from us now, then what has been sent.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Query, State};
use axum::response::Response;
use koan_kit_web as kit;
use slsk_engine::slsk_proto::RawStr;

use super::{Body, Page, Tab};
use crate::App;
use crate::graphql::Transfer;
use crate::web::{Draw, MOST, UiState, human, place_url};

pub(in crate::web) struct Uploads;

#[derive(Template)]
#[template(path = "uploads/page.html")]
struct UploadsPage {
    peers: Html,
    history: Html,
}

impl Page for Uploads {
    const TAB: Tab = Tab::Uploads;
    const STREAM: &'static str = "/stream/uploads";
    type Args = ();

    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&UploadsPage {
            peers: Peers::now(&s.app).draw(),
            history: History::newest(&s.app, crate::web::PAGE).await.draw(),
        }))
    }
}

/// Who is taking what from us, grouped by person: the fastest first.
#[derive(Template)]
#[template(path = "uploads/peers.html")]
pub(in crate::web) struct Peers {
    peers: Vec<Peer>,
    /// When the transfers were read, for `live.js` to draw from (ms since
    /// the epoch).
    at: i64,
    sending: usize,
    queued: usize,
    sent: usize,
    shared: usize,
}

/// One person taking files from us.
struct Peer {
    username: String,
    files: Vec<Transfer>,
    done: usize,
    speed: u64,
}

impl Peers {
    pub(in crate::web) fn now(app: &App) -> Self {
        let mut view = Self {
            peers: Vec::new(),
            at: chrono::Utc::now().timestamp_millis(),
            sending: 0,
            queued: 0,
            sent: 0,
            shared: 0,
        };
        let Some(e) = app.session.engine() else {
            return view;
        };
        view.shared = e.share_counts().1;
        let mut by_user: std::collections::BTreeMap<String, Peer> = Default::default();
        for t in e.uploads() {
            let peer = by_user.entry(t.username.clone()).or_insert_with(|| Peer {
                username: t.username.clone(),
                files: Vec::new(),
                done: 0,
                speed: 0,
            });
            match t.state {
                "completed" => {
                    peer.done += 1;
                    view.sent += 1;
                }
                "cancelled" | "failed" => {}
                state => {
                    if state == "transferring" {
                        view.sending += 1;
                    } else {
                        view.queued += 1;
                    }
                    peer.speed += t.speed;
                    peer.files.push(Transfer::from(t));
                }
            }
        }
        view.peers = by_user
            .into_values()
            .filter(|p| !p.files.is_empty())
            .collect();
        view.peers.sort_by_key(|p| std::cmp::Reverse(p.speed));
        view
    }

    fn leaf<'a>(&self, name: &'a str) -> &'a str {
        crate::web::leaf(name)
    }
    fn cancel(&self, t: &Transfer) -> String {
        place_url(
            "/uploads/cancel",
            &[("username", &t.username), ("filename", &t.filename)],
        )
    }
    /// What has gone, carrying the measurement `live.js` moves it from.
    fn meter(&self, t: &Transfer) -> kit::Meter<'static> {
        kit::Meter::new(t.bytes, t.size.max(1))
            .label("upload")
            .data("bytes", t.bytes)
            .data("total", t.size)
            .data("rate", t.speed)
            .data("at", self.at)
    }
    fn size(&self, b: &u64) -> String {
        human(*b)
    }
    fn label(&self, t: &Transfer) -> String {
        match (t.state.as_str(), t.place) {
            ("queued", Some(p)) => format!("queued #{p}"),
            ("queued", None) => "queued".into(),
            ("transferring", _) => "sending".into(),
            (s, _) => s.into(),
        }
    }
}

/// What has been sent, and the day's and week's totals.
#[derive(Template)]
#[template(path = "uploads/history.html")]
pub(in crate::web) struct History {
    history: Vec<crate::db::UploadRow>,
    /// Older history exists beyond that shown.
    more: bool,
    /// Completed uploads today and this week: (files, bytes).
    today: (i64, i64),
    week: (i64, i64),
}

impl History {
    /// The newest `shown` uploads finished.
    pub(in crate::web) async fn newest(app: &App, shown: i64) -> Self {
        let now = chrono::Utc::now();
        let midnight = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc())
            .unwrap_or(now);
        let shown = shown.clamp(1, MOST);
        let mut history = crate::db::recent_uploads(&app.db, shown + 1)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "could not read upload history"))
            .unwrap_or_default();
        let more = shown < MOST && history.len() as i64 > shown;
        history.truncate(shown as usize);
        let (week, today) =
            crate::db::upload_totals(&app.db, now - chrono::Duration::days(7), midnight)
                .await
                .unwrap_or_default();
        Self {
            history,
            more,
            today,
            week,
        }
    }

    fn bytes(&self, b: &i64) -> String {
        human((*b).max(0) as u64)
    }
    fn files(&self, n: &i64) -> String {
        if *n == 1 {
            "1 file".into()
        } else {
            format!("{n} files")
        }
    }
    /// Average speed over the time it was seen sending, when that is known.
    fn rate(&self, h: &crate::db::UploadRow) -> String {
        match h.seconds.filter(|s| *s >= 1.0) {
            Some(s) => format!("{}/s", human((h.bytes.max(0) as f64 / s) as u64)),
            None => String::new(),
        }
    }
    fn ago(&self, h: &crate::db::UploadRow) -> String {
        crate::web::ago(h.finished_at)
    }
    fn leaf<'a>(&self, name: &'a str) -> &'a str {
        crate::web::leaf(name)
    }
    fn page(&self) -> i64 {
        crate::web::PAGE
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct UploadQuery {
    username: String,
    filename: String,
}

/// One upload stopped; the stream draws it gone.
pub(in crate::web) async fn cancel(
    State(s): State<UiState>,
    Query(q): Query<UploadQuery>,
) -> Response {
    if let Some(e) = s.app.session.engine() {
        e.cancel_upload(&q.username, &RawStr::from(q.filename));
    }
    askama_datastar::events([kit::Flash::region().patch().into()])
}
