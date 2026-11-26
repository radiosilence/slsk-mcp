//! A job's page live: the page redrawn whole as the job changes, since it is
//! one card and its history.

use axum::response::sse::Event;
use uuid::Uuid;

use super::{Changed, Last, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::downloads::job::{JobPage, of_path};
use crate::web::{Draw, UiState};

pub(in crate::web) struct Job {
    id: Option<Uuid>,
    page: Last,
}

impl Live for Job {
    const TAB: Tab = Tab::Downloads;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            id: of_path(shown.uri().path()),
            page: Last::default(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let Some(id) = self.id else {
            return Vec::new();
        };
        if !(c.topic(Topic::Jobs) || c.moved || c.minute) {
            return Vec::new();
        }
        match JobPage::of(&s.app, id).await {
            Ok((page, _)) => self.page.send(page.draw()).into_iter().collect(),
            Err(e) => {
                tracing::warn!(%id, error = %e, "could not show job");
                Vec::new()
            }
        }
    }
}
