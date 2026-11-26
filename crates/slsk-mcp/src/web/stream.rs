//! The live streams: one per page, each keeping its page current. Every
//! stream has the same shape ([`Live`]): it opens from what the page shows
//! (its signals, the address among them), draws the regions a change touches,
//! and may have changes of its own to wait on (a search's answers, a peer's
//! reply). [`serve`] runs any of them, with the status line and the nav that
//! every page shows, and nothing is polled: it waits on what changes them (a
//! database write, an import's stage, a transfer's state or measured rate,
//! the connection, the minute turning), paced by `askama_datastar::paced`:
//! at most one redraw every quarter second, and the stream ended after a
//! minute for the page's `reopen` attributes to open another, which bounds
//! how long a page sits on a connection that died without either end
//! noticing (a suspended phone, a replaced pod). A region is sent only when
//! it differs from what was last sent (`askama_datastar::Last`). The page
//! also opens its stream afresh when its address changes, so a stream always
//! draws the page the address names.

use std::future::Future;
use std::time::Duration;

use askama_datastar::{Last, Pace, Signals, paced};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::Event;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use tokio::sync::watch;

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

/// A page's live stream.
pub(super) trait Live: Sized + Send + 'static {
    /// The tab whose page it keeps current.
    const TAB: Tab;
    /// Ready to draw the page `shown` describes.
    fn open(s: &UiState, shown: Shown) -> impl Future<Output = Self> + Send;
    /// The regions `changed` touches, drawn and sent if they differ from what
    /// was last sent.
    fn draw(&mut self, s: &UiState, changed: &Changed) -> impl Future<Output = Vec<Event>> + Send;
    /// Changes of the stream's own, beside the app's: a running search's
    /// answers, a card due to be sent again. Each wakes the stream with
    /// [`Changed::woke`] set.
    fn wakes(&self) -> Option<BoxStream<'static, ()>> {
        None
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
    /// The library list's rows the page holds, beyond the first page.
    pub(super) librows: usize,
    /// The live topics' counts the page has drawn up to: as they stood when
    /// its body was drawn, then as each pass of its stream left them. None
    /// from a page that does not say, which is drawn whole.
    pub(super) seen: Option<crate::live::Counts>,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            url: String::new(),
            jobs: super::PAGE,
            history: super::PAGE,
            files: String::new(),
            libseen: 0,
            librows: 0,
            seen: None,
        }
    }
}

impl Shown {
    /// The page's path, without its query.
    pub(super) fn path(&self) -> &str {
        self.url.split(['?', '#']).next().unwrap_or_default()
    }

    /// The view the address's query describes.
    pub(super) fn view<T: serde::de::DeserializeOwned + Default>(&self) -> T {
        askama_datastar::View::<T>::of(&self.url).0
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
    /// A change of the stream's own ([`Live::wakes`]).
    pub(super) woke: bool,
    /// The topics' counts this redraw brings the page up to.
    seen: crate::live::Counts,
}

impl Changed {
    pub(super) fn topic(&self, t: Topic) -> bool {
        self.topics[t as usize]
    }
}

/// A region as last sent, compared without the names the kit gives afresh on
/// every render ([`unnamed`]).
pub(super) fn last() -> Last {
    Last::comparing(unnamed)
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
    /// Private messages not yet read, read again only when messages change.
    unread: Option<i64>,
}

impl Header {
    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.connection || c.moved || c.minute {
            events.extend(self.status.send(&Status::of(&s.app).draw()));
        }
        if self.unread.is_none() || c.topic(Topic::Social) || c.minute {
            self.unread = Some(s.app.social.unread().await.unwrap_or(0));
        }
        if c.moved || c.minute || c.topic(Topic::Social) {
            let nav = Nav::new(&s.app, self.tab, self.unread.unwrap_or(0));
            events.extend(self.nav.send(&nav.draw()));
        }
        events
    }
}

/// What changes the page: the live topics, transfers, the connection, the
/// minute turning, and the stream's own wakes. The first item is what moved
/// after the page was drawn; each after it gathers whatever came in since the
/// one before, as the watches it reads keep only the latest.
struct Sources {
    changes: watch::Receiver<crate::live::Counts>,
    transfers: watch::Receiver<u64>,
    status: Option<watch::Receiver<slsk_engine::Status>>,
    wakes: Option<BoxStream<'static, ()>>,
    seen: crate::live::Counts,
    first: Option<Changed>,
}

impl Sources {
    fn new(
        s: &UiState,
        drawn: Option<crate::live::Counts>,
        wakes: Option<BoxStream<'static, ()>>,
    ) -> Self {
        let mut changes = crate::live::subscribe();
        let seen = *changes.borrow_and_update();
        // A page that says what it drew gets only what moved since; one that
        // does not is drawn whole.
        let first = Changed {
            topics: match drawn {
                Some(then) => std::array::from_fn(|i| seen[i] != then[i]),
                None => [true; TOPICS],
            },
            moved: drawn.is_none(),
            connection: drawn.is_none(),
            minute: drawn.is_none(),
            woke: false,
            seen,
        };
        Self {
            changes,
            transfers: slsk_engine::changes(),
            status: s.app.session.engine().map(|e| e.status()),
            wakes,
            seen,
            first: Some(first),
        }
    }

    async fn next(&mut self) -> Option<Changed> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        let next_minute = tokio::time::Instant::now()
            + Duration::from_secs(60 - (chrono::Utc::now().timestamp() % 60) as u64);
        let (mut moved, mut connection, mut minute, mut woke) = (false, false, false, false);
        tokio::select! {
            r = self.changes.changed() => r.ok()?,
            r = self.transfers.changed() => {
                r.ok()?;
                moved = true;
            }
            Some(Ok(())) = async {
                match self.status.as_mut() {
                    Some(rx) => Some(rx.changed().await),
                    None => std::future::pending().await,
                }
            } => connection = true,
            () = tokio::time::sleep_until(next_minute) => minute = true,
            Some(()) = async {
                match self.wakes.as_mut() {
                    Some(w) => w.next().await,
                    None => std::future::pending().await,
                }
            } => woke = true,
        }
        // Whatever else came in meanwhile is drawn in the same pass.
        let now = *self.changes.borrow_and_update();
        let topics = std::array::from_fn(|i| now[i] != self.seen[i]);
        self.seen = now;
        if self.transfers.has_changed().unwrap_or(false) {
            self.transfers.mark_unchanged();
            moved = true;
        }
        if let Some(rx) = self.status.as_mut()
            && rx.has_changed().unwrap_or(false)
        {
            rx.mark_unchanged();
            connection = true;
        }
        Some(Changed {
            topics,
            moved,
            connection,
            minute,
            woke,
            seen: now,
        })
    }
}

/// Run stream `L` for the page that sent `shown`.
pub(super) async fn serve<L: Live>(
    State(s): State<UiState>,
    Signals(shown): Signals<Shown>,
) -> impl IntoResponse {
    let drawn = shown.seen;
    let mut page = L::open(&s, shown).await;
    let sources = Sources::new(&s, drawn, page.wakes());
    let changes = futures_util::stream::unfold(sources, |mut sources| async move {
        let changed = sources.next().await?;
        Some((changed, sources))
    });
    let events = async_stream::stream! {
        let mut header = Header { tab: L::TAB, status: last(), nav: last(), unread: None };
        let mut changes = std::pin::pin!(paced(changes, Pace::default()));
        // Said on the first pass whatever else is sent, so the page hears from
        // a stream as soon as it opens.
        let mut reported = None;
        while let Some(changed) = changes.next().await {
            for e in header.draw(&s, &changed).await {
                yield e;
            }
            for e in page.draw(&s, &changed).await {
                yield e;
            }
            // The page now has everything up to these counts, so a stream
            // opened after this one carries on from them.
            if reported != Some(changed.seen) {
                yield super::signals(serde_json::json!({ "seen": changed.seen.to_vec() }));
                reported = Some(changed.seen);
            }
        }
    };
    askama_datastar::stream(events)
}
