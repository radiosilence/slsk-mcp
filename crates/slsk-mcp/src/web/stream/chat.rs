//! The Chat page live: what is said in the open conversation as it is said,
//! and the lists beside it (rooms, conversations and their unread counts,
//! buddies and whether they are online).

use axum::response::sse::Event;

use super::{Changed, Last, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::chat::{ChatQuery, Lines, Side, Thread};
use crate::web::{Draw, UiState};

pub(in crate::web) struct Chat {
    thread: Option<Thread>,
    lines: Last,
    side: Last,
}

impl Live for Chat {
    const TAB: Tab = Tab::Chat;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            thread: shown.view::<ChatQuery>().thread(),
            lines: Last::default(),
            side: Last::default(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        if !(c.topic(Topic::Social) || c.minute) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // The lines first: drawing a private conversation marks it read,
        // which the lists then reflect.
        if let Some(t) = &self.thread {
            events.extend(self.lines.send(Lines::of(&s.app, t).await.draw()));
        }
        events.extend(self.side.send(Side::of(&s.app).await.draw()));
        events
    }
}
