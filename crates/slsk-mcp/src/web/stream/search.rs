//! The Search page live: the tab strip as searches open, finish and close,
//! and the tab shown, redrawn as peers answer while its search runs, when it
//! finishes, and as the server says who is online. The stream follows a
//! search already running and never starts one: the page and its actions
//! do (see [`crate::web::page::search::run`]).

use axum::response::sse::Event;
use tokio::sync::watch;

use super::{Changed, Last, Live, Shown};
use crate::db::SearchTab;
use crate::graphql::Heard;
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::search::{Results, SearchQuery, Tabs};
use crate::web::{Draw, UiState};

pub(in crate::web) struct Search {
    view: SearchQuery,
    strip: Last,
    results: Last,
    /// The running search shown, while it runs.
    heard: Option<watch::Receiver<Heard>>,
    /// Peers have answered since the results were last drawn.
    answered: bool,
}

impl Live for Search {
    const TAB: Tab = Tab::Search;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let view: SearchQuery = shown.view();
        let heard = tab(s, &view)
            .await
            .filter(|t| t.searched_at.is_none())
            .and_then(|t| crate::graphql::listening(&t.query));
        Self {
            view,
            strip: Last::default(),
            results: Last::default(),
            heard,
            answered: false,
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.topic(Topic::Search) || c.minute {
            let tabs = Tabs::of(&s.app, self.view.tab()).await;
            events.extend(self.strip.send(tabs.draw()));
        }
        let answered = std::mem::take(&mut self.answered);
        // The tab finishing, or the server saying who is online, redraws
        // what it kept; peers answering redraws what is heard so far.
        if !(answered || c.topic(Topic::Search) || c.topic(Topic::Peers)) {
            return events;
        }
        let Some(tab) = tab(s, &self.view).await else {
            return events;
        };
        let results = match (
            &tab.searched_at,
            self.heard.as_mut(),
            s.app.session.engine(),
        ) {
            (Some(_), ..) => {
                self.heard = None;
                Results::kept(s, &tab, self.view.shown()).await
            }
            (None, Some(heard), Some(engine)) if answered || c.topic(Topic::Peers) => {
                let now = heard.borrow_and_update().clone();
                Results::heard(s, engine, &tab, &now, self.view.shown())
            }
            _ => return events,
        };
        events.extend(self.results.send(results.draw()));
        events
    }

    async fn wake(&mut self) {
        let Some(heard) = self.heard.as_mut() else {
            return std::future::pending().await;
        };
        // A search that went away is drawn as it ended, from what it kept.
        if heard.changed().await.is_err() {
            self.heard = None;
        }
        self.answered = true;
    }
}

/// The tab the address names, as it now stands.
async fn tab(s: &UiState, view: &SearchQuery) -> Option<SearchTab> {
    crate::db::search_tab(&s.app.db, view.tab()?)
        .await
        .ok()
        .flatten()
}
