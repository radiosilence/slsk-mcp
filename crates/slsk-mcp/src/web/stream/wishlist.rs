//! The Wishlist page live: the wishes as they are searched and become albums.

use axum::response::sse::Event;

use super::{Changed, Live, Shown, last};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::wishlist::Wishes;
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Wishlist {
    list: Last,
}

impl Live for Wishlist {
    const TAB: Tab = Tab::Wishlist;

    async fn open(_: &UiState, _: Shown) -> Self {
        Self { list: last() }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        if !(c.topic(Topic::Social) || c.topic(Topic::Jobs) || c.minute) {
            return Vec::new();
        }
        self.list
            .send(&Wishes::of(&s.app).await.draw())
            .into_iter()
            .collect()
    }
}
