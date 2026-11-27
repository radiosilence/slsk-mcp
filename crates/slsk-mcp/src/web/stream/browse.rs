//! The Browse page live: a peer's shares, drawn once they answer. The page
//! asks them (see [`crate::web::peer::Peers`]); the stream only draws the
//! answer when it lands.

use axum::response::sse::Event;

use super::{Changed, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::browse::{BrowseQuery, Place, Shares};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Browse {
    place: Option<Place>,
    /// When the answer last drawn came.
    drawn: Option<std::time::Instant>,
    shares: Last,
}

impl Live for Browse {
    const TAB: Tab = Tab::Browse;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            place: Place::from_query(&shown.view::<BrowseQuery>()).ok(),
            drawn: None,
            shares: Last::new(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let (Some(p), Some(engine)) = (&self.place, s.app.session.engine()) else {
            return Vec::new();
        };
        let answered = s.peers.shares.answered_at(&p.username);
        if !c.topic(Topic::Peers) || answered.is_none() || answered == self.drawn {
            return Vec::new();
        }
        self.drawn = answered;
        self.shares
            .send(&Shares::of(s, engine, p).draw())
            .into_iter()
            .collect()
    }
}
