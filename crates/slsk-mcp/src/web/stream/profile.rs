//! The Profile page live: the card, drawn again as what the server says of
//! the user, and for a peer what they say of themselves, arrive. The page
//! asks both (see [`crate::web::peer::Peers`]); the stream only draws the
//! answers.

use std::time::Instant;

use axum::response::sse::Event;

use super::{Changed, Live, Shown, last};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::profile::{ProfileQuery, card};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Profile {
    username: String,
    /// When the answers last drawn came: the server's, then the peer's.
    drawn: (Option<Instant>, Option<Instant>),
    card: Last,
}

impl Live for Profile {
    const TAB: Tab = Tab::Profile;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let q: ProfileQuery = shown.view();
        let username = Some(q.username.trim().to_string())
            .filter(|u| !u.is_empty())
            .or_else(|| s.app.session.engine().map(|e| e.username()))
            .unwrap_or_default();
        Self {
            username,
            drawn: (None, None),
            card: last(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let Some(engine) = s.app.session.engine() else {
            return Vec::new();
        };
        let answered = (
            s.peers.server.answered_at(&self.username),
            s.peers.info.answered_at(&self.username),
        );
        if !c.topic(Topic::Peers) || answered == self.drawn {
            return Vec::new();
        }
        self.drawn = answered;
        let card = card(s, engine, &self.username).await;
        self.card.send(&card.draw()).into_iter().collect()
    }
}
