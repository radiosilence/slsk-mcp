//! A job's page live: the page redrawn whole as the job changes, since it is
//! one card and its history.

use axum::response::sse::Event;
use uuid::Uuid;

use super::{Changed, Live, Shown};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::downloads::job::{JobPage, of_path};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Job {
    id: Option<Uuid>,
    page: Last,
    /// The last pass could not read the job.
    failed: bool,
}

impl Live for Job {
    const TAB: Tab = Tab::Downloads;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            id: of_path(shown.path()),
            page: Last::new(),
            failed: false,
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        self.failed = false;
        let Some(id) = self.id else {
            return Vec::new();
        };
        if !(c.topic(Topic::Jobs) || c.moved) {
            return Vec::new();
        }
        match JobPage::of(&s.app, id).await {
            Ok((page, _)) => self.page.send(&page.draw()).into_iter().collect(),
            Err(e) => {
                tracing::warn!(%id, error = %e, "could not show job");
                self.failed = true;
                Vec::new()
            }
        }
    }

    fn failed(&self) -> bool {
        self.failed
    }
}
