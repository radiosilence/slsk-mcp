//! The Downloads page live: the jobs in hand card by card, and the jobs done
//! with. A card that changed is sent alone; the whole list only when a job
//! comes, goes or moves section. The jobs done with are read only when a job
//! settles, since none of them move.

use std::collections::HashMap;
use std::time::Duration;

use axum::response::sse::Event;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Changed, Live, Shown, last, unnamed};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::downloads::{Live as Jobs, Settled};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

/// Least time between two sends of one card for its progress alone; the
/// browser moves the bars in between.
const CARD_EVERY: Duration = Duration::from_secs(1);

pub(in crate::web) struct Downloads {
    shown: Shown,
    settled: Option<Settled>,
    /// The list's shape as last sent, and the list itself.
    shape: String,
    list: Last,
    /// Each card as last sent, and when.
    cards: HashMap<String, (String, Instant)>,
    /// When a card held back by [`CARD_EVERY`] is due, which wakes the stream.
    recheck: watch::Sender<Option<Instant>>,
}

impl Live for Downloads {
    const TAB: Tab = Tab::Downloads;

    async fn open(_: &crate::web::UiState, shown: Shown) -> Self {
        Self {
            shown,
            settled: None,
            shape: String::new(),
            list: last(),
            cards: HashMap::new(),
            recheck: watch::Sender::new(None),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        let jobs = c.topic(Topic::Jobs);
        if jobs || c.minute {
            let settled = Settled::newest(&s.app, self.shown.jobs).await;
            if self.settled.as_ref() != Some(&settled) {
                events.push(settled.draw().patch().into());
                self.settled = Some(settled);
            }
        }
        if !(jobs || c.moved || c.minute || c.woke) {
            return events;
        }
        // The page drew the jobs done with; read them only to know whether
        // an empty list says "Nothing yet".
        let settled = match &self.settled {
            Some(settled) => settled,
            None => self
                .settled
                .insert(Settled::newest(&s.app, self.shown.jobs).await),
        };
        let any_settled = !settled.jobs.is_empty();
        let live = Jobs::of(&s.app, self.shown.jobs, &self.shown.files, any_settled).await;
        let now = Instant::now();
        let mut recheck: Option<Instant> = None;
        let shape = live.shape();
        if shape != self.shape {
            self.cards = live
                .attention
                .iter()
                .chain(&live.underway)
                .map(|card| (card.j.id.to_string(), (unnamed(&card.draw()), now)))
                .collect();
            events.extend(self.list.send(&live.draw()));
            self.shape = shape;
            self.recheck.send_replace(None);
            return events;
        }
        for card in live.attention.iter().chain(&live.underway) {
            let html = card.draw();
            let compared = unnamed(&html);
            let id = card.j.id.to_string();
            let sent = self.cards.get(&id);
            if sent.is_some_and(|(h, _)| *h == compared) {
                continue;
            }
            // Progress alone is sent once a second per card; a change in the
            // job itself goes at once.
            if !jobs
                && let Some((_, at)) = sent
                && now < *at + CARD_EVERY
            {
                let due = *at + CARD_EVERY;
                recheck = Some(recheck.map_or(due, |r| r.min(due)));
                continue;
            }
            events.push(html.patch().into());
            self.cards.insert(id, (compared, now));
            // The list as last sent no longer matches the page.
            self.list = last();
        }
        self.recheck.send_replace(recheck);
        events
    }

    /// Each time a card held back comes due.
    fn wakes(&self) -> Option<BoxStream<'static, ()>> {
        let due = self.recheck.subscribe();
        Some(
            futures_util::stream::unfold(due, |mut due| async move {
                loop {
                    let at = *due.borrow_and_update();
                    match at {
                        Some(at) => tokio::select! {
                            () = tokio::time::sleep_until(at) => return Some(((), due)),
                            r = due.changed() => r.ok()?,
                        },
                        None => due.changed().await.ok()?,
                    }
                }
            })
            .boxed(),
        )
    }
}
