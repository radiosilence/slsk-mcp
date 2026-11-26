//! The Downloads page live: the jobs in hand card by card, and the jobs done
//! with. A card that changed is sent alone; the whole list only when a job
//! comes, goes or moves section. The jobs done with are read only when a job
//! settles, since none of them move.

use std::collections::HashMap;
use std::time::Duration;

use axum::response::sse::Event;
use tokio::time::Instant;

use super::{Changed, Last, Live, Shown, unnamed};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::downloads::{Live as Jobs, Settled};
use crate::web::{Draw, UiState};

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
    /// When a card held back by [`CARD_EVERY`] is due.
    recheck: Option<Instant>,
}

impl Live for Downloads {
    const TAB: Tab = Tab::Downloads;

    async fn open(_: &crate::web::UiState, shown: Shown) -> Self {
        Self {
            shown,
            settled: None,
            shape: String::new(),
            list: Last::default(),
            cards: HashMap::new(),
            recheck: None,
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        let jobs = c.topic(Topic::Jobs);
        if jobs || c.minute || self.settled.is_none() {
            let settled = Settled::newest(&s.app, self.shown.jobs).await;
            if self.settled.as_ref() != Some(&settled) {
                events.push(settled.draw().patch().into());
                self.settled = Some(settled);
            }
        }
        if !(jobs || c.moved || c.minute || self.recheck.is_some()) {
            return events;
        }
        let any_settled = self.settled.as_ref().is_some_and(|s| !s.jobs.is_empty());
        let live = Jobs::of(&s.app, self.shown.jobs, &self.shown.files, any_settled).await;
        let now = Instant::now();
        self.recheck = None;
        let shape = live.shape();
        if shape != self.shape {
            self.cards = live
                .attention
                .iter()
                .chain(&live.underway)
                .map(|card| (card.j.id.to_string(), (unnamed(&card.draw()), now)))
                .collect();
            events.extend(self.list.send(live.draw()));
            self.shape = shape;
            return events;
        }
        for card in live.attention.iter().chain(&live.underway) {
            let html = card.draw();
            let compared = unnamed(&html);
            let id = card.j.id.to_string();
            let last = self.cards.get(&id);
            if last.is_some_and(|(h, _)| *h == compared) {
                continue;
            }
            // Progress alone is sent once a second per card; a change in the
            // job itself goes at once.
            if !jobs
                && let Some((_, at)) = last
                && now < *at + CARD_EVERY
            {
                let due = *at + CARD_EVERY;
                self.recheck = Some(self.recheck.map_or(due, |r| r.min(due)));
                continue;
            }
            events.push(html.patch().into());
            self.cards.insert(id, (compared, now));
            // The list as last sent no longer matches the page.
            self.list = Last::default();
        }
        events
    }

    async fn wake(&mut self) {
        match self.recheck {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    }
}
