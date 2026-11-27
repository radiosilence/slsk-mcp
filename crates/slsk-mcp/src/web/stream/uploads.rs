//! The Uploads page live: who is taking what, redrawn as transfers move, and
//! what has been sent, read again only when an upload is recorded.

use axum::response::sse::Event;

use super::{Changed, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::uploads::{History, Peers};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Uploads {
    shown: i64,
    peers: Last,
    history: Last,
}

impl Live for Uploads {
    const TAB: Tab = Tab::Uploads;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            shown: shown.history,
            peers: Last::new(),
            history: Last::new(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.moved || c.minute {
            events.extend(self.peers.send(&Peers::now(&s.app).draw()));
        }
        if c.topic(Topic::Uploads) || c.minute {
            let history = History::newest(&s.app, self.shown).await;
            events.extend(self.history.send(&history.draw()));
        }
        events
    }
}
