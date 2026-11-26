//! The live streams: one per page, each keeping its page current. Every
//! stream has the same shape ([`Live`]): it opens from what the page shows
//! (its signals, the address among them), draws the regions a change touches,
//! and may have changes of its own to wait on (a search's answers, a peer's
//! reply). [`serve`] runs any of them, with the status line and the nav that
//! every page shows, and nothing is polled: it waits on what changes them (a
//! database write, an import's stage, a transfer's state or measured rate,
//! the connection, the minute turning) and redraws at most every
//! [`THROTTLE`], sending a region only when it differs from what was sent.
//!
//! A stream ends itself after [`LIFETIME`], and the page's `reopen`
//! attributes open it again within seconds, as they do one that ends or goes
//! quiet for any other reason: a stream can die without either end noticing
//! (a suspended phone, a replaced pod), and ending it on a timer bounds how
//! long a page can sit showing stale state. The page also opens it afresh
//! when its address changes, so a stream always draws the page the address
//! names.

use std::future::Future;
use std::time::Duration;

use askama_datastar::{Html, Signals};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::Event;

use super::layout::{Nav, Status};
use super::page::Tab;
use super::{Draw, UiState};
use crate::live::{TOPICS, Topic};

pub(super) mod activity;
pub(super) mod browse;
pub(super) mod chat;
pub(super) mod downloads;
pub(super) mod job;
pub(super) mod library;
pub(super) mod profile;
pub(super) mod search;
pub(super) mod uploads;
pub(super) mod wishlist;

/// Least time between two redraws of a page. Changes coming faster are
/// gathered into the next one; bars and countdowns move in the browser in
/// between, from the measurement and time each patch carries.
const THROTTLE: Duration = Duration::from_millis(250);
/// How long a stream lives before the page opens another.
const LIFETIME: Duration = Duration::from_secs(60);

/// A page's live stream.
pub(super) trait Live: Sized + Send + 'static {
    /// The tab whose page it keeps current.
    const TAB: Tab;
    /// Ready to draw the page `shown` describes.
    fn open(s: &UiState, shown: Shown) -> impl Future<Output = Self> + Send;
    /// The regions `changed` touches, drawn and sent if they differ from what
    /// was last sent. Everything on the first call.
    fn draw(&mut self, s: &UiState, changed: &Changed) -> impl Future<Output = Vec<Event>> + Send;
    /// Resolves when the stream has something of its own to draw.
    fn wake(&mut self) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
}

/// A page with nothing live of its own: the status line and nav alone.
pub(super) struct Quiet<const TAB: u8>;

/// The tab a [`Quiet`] stream serves, by its place in [`Tab::ALL`].
pub(super) const fn tab(t: Tab) -> u8 {
    let mut i = 0;
    while i < Tab::ALL.len() {
        if Tab::ALL[i] as u8 == t as u8 {
            return i as u8;
        }
        i += 1;
    }
    0
}

impl<const TAB: u8> Live for Quiet<TAB> {
    const TAB: Tab = Tab::ALL[TAB as usize];
    async fn open(_: &UiState, _: Shown) -> Self {
        Self
    }
    async fn draw(&mut self, _: &UiState, _: &Changed) -> Vec<Event> {
        Vec::new()
    }
}

/// What the page shows, from the signals Datastar sends with the stream's
/// request: its address, and what of the page lives outside it.
#[derive(serde::Deserialize)]
#[serde(default)]
pub(super) struct Shown {
    /// The page's address, path and query.
    pub(super) url: String,
    /// Rows of the long lists shown: each kind of job, and upload history.
    pub(super) jobs: i64,
    pub(super) history: i64,
    /// The jobs whose files are open, each followed by a comma.
    pub(super) files: String,
    /// The last library run whose changes the Library page has drawn; 0 for
    /// a page that has not drawn its list yet.
    pub(super) libseen: u64,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            url: String::new(),
            jobs: super::PAGE,
            history: super::PAGE,
            files: String::new(),
            libseen: 0,
        }
    }
}

impl Shown {
    pub(super) fn uri(&self) -> http::Uri {
        self.url.parse().unwrap_or_default()
    }

    /// The view the address's query describes.
    pub(super) fn view<T: serde::de::DeserializeOwned + Default>(&self) -> T {
        super::query(&self.uri())
    }
}

/// What changed since the last redraw.
pub(super) struct Changed {
    topics: [bool; TOPICS],
    /// A transfer moved or changed state.
    pub(super) moved: bool,
    /// The session's connection changed.
    pub(super) connection: bool,
    /// The minute turned, so times shown as "3 min ago" are redrawn.
    pub(super) minute: bool,
}

impl Changed {
    fn everything() -> Self {
        Self {
            topics: [true; TOPICS],
            moved: true,
            connection: true,
            minute: true,
        }
    }

    pub(super) fn topic(&self, t: Topic) -> bool {
        self.topics[t as usize]
    }
}

/// A region as last sent, to send it again only when it differs.
#[derive(Default)]
pub(super) struct Last(String);

impl Last {
    /// `html` as a patch, unless it is what was last sent.
    pub(super) fn send(&mut self, html: Html) -> Option<Event> {
        let compared = unnamed(&html);
        (compared != self.0).then(|| {
            self.0 = compared;
            html.patch().into()
        })
    }
}

/// Markup as a stream compares it with what it last sent. The kit names a request button's own busy signal afresh on
/// every render, keyed or not, so a region drawn twice from the same state differs only in those names; they are left
/// out, or every redraw would be sent again. A region sent for another change brings its new names with it.
pub(super) fn unnamed(html: &str) -> String {
    const SIGNAL: &str = "_busy";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(SIGNAL) {
        let (before, after) = rest.split_at(at + SIGNAL.len());
        out.push_str(before);
        let hex = after
            .bytes()
            .take(12)
            .take_while(u8::is_ascii_hexdigit)
            .count();
        rest = if hex == 12 { &after[12..] } else { after };
    }
    out.push_str(rest);
    out
}

/// The status line and the nav, on every page.
struct Header {
    tab: Tab,
    status: Last,
    nav: Last,
}

impl Header {
    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.connection || c.moved || c.minute {
            events.extend(self.status.send(Status::of(&s.app).draw()));
        }
        if c.moved || c.minute || c.topic(Topic::Social) {
            events.extend(self.nav.send(Nav::of(&s.app, self.tab).await.draw()));
        }
        events
    }
}

/// Run stream `L` for the page that sent `shown`.
pub(super) async fn serve<L: Live>(
    State(s): State<UiState>,
    Signals(shown): Signals<Shown>,
) -> impl IntoResponse {
    let events = async_stream::stream! {
        let mut page = L::open(&s, shown).await;
        let mut header = Header { tab: L::TAB, status: Last::default(), nav: Last::default() };
        let mut changes = crate::live::subscribe();
        let mut transfers = slsk_engine::changes();
        let mut status = s.app.session.engine().map(|e| e.status());
        let ends = tokio::time::Instant::now() + LIFETIME;
        let mut seen = *changes.borrow_and_update();
        let mut changed = Changed::everything();
        loop {
            for e in header.draw(&s, &changed).await {
                yield e;
            }
            for e in page.draw(&s, &changed).await {
                yield e;
            }
            tokio::time::sleep(THROTTLE).await;
            let next_minute = tokio::time::Instant::now()
                + Duration::from_secs(60 - (chrono::Utc::now().timestamp() % 60) as u64);
            changed = Changed { topics: [false; TOPICS], moved: false, connection: false, minute: false };
            tokio::select! {
                r = changes.changed() => if r.is_err() { break },
                r = transfers.changed() => match r {
                    Ok(()) => changed.moved = true,
                    Err(_) => break,
                },
                Some(Ok(())) = async {
                    match status.as_mut() {
                        Some(rx) => Some(rx.changed().await),
                        None => std::future::pending().await,
                    }
                } => changed.connection = true,
                () = tokio::time::sleep_until(next_minute) => changed.minute = true,
                () = tokio::time::sleep_until(ends) => break,
                () = page.wake() => {}
            }
            // Whatever else came in meanwhile is drawn in the same pass.
            let now = *changes.borrow_and_update();
            changed.topics = std::array::from_fn(|i| now[i] != seen[i]);
            seen = now;
            if transfers.has_changed().unwrap_or(false) {
                transfers.mark_unchanged();
                changed.moved = true;
            }
            if let Some(rx) = status.as_mut()
                && rx.has_changed().unwrap_or(false)
            {
                rx.mark_unchanged();
                changed.connection = true;
            }
        }
    };
    askama_datastar::stream(events)
}
