//! The Activity page live: the feed as other users come by, and the week's
//! totals each minute.

use axum::response::sse::Event;

use super::{Changed, Last, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::activity::{Feed, Trends};
use crate::web::{Draw, UiState};

#[derive(Default)]
pub(in crate::web) struct Activity {
    feed: Last,
    trends: Last,
}

impl Live for Activity {
    const TAB: Tab = Tab::Activity;

    async fn open(_: &UiState, _: Shown) -> Self {
        Self::default()
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.topic(Topic::Activity) || c.minute {
            events.extend(self.feed.send(Feed::now(&s.app).draw()));
        }
        if c.minute {
            events.extend(self.trends.send(Trends::of(&s.app).await.draw()));
        }
        events
    }
}
