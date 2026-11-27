//! The Search page live: the tab strip as searches open, finish and close,
//! and the tab shown, redrawn as peers answer while its search runs, when it
//! finishes, and as the server says who is online. The stream follows a
//! search already running and never starts one: the page and its actions
//! do (see [`crate::web::page::search::run`]).

use axum::response::sse::Event;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use tokio::sync::watch;

use super::{Changed, Live, Shown};
use crate::db::SearchTab;
use crate::graphql::Heard;
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::search::{Results, SearchQuery, Tabs};
use crate::web::{Draw, UiState};
use askama_datastar::Last;

pub(in crate::web) struct Search {
    view: SearchQuery,
    strip: Last,
    results: Last,
    /// The running search shown, while it runs.
    heard: Option<watch::Receiver<Heard>>,
    /// The last pass could not read the tabs.
    failed: bool,
}

impl Live for Search {
    const TAB: Tab = Tab::Search;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let view: SearchQuery = shown.view();
        let heard = tab(s, &view)
            .await
            .ok()
            .flatten()
            .filter(|t| t.searched_at.is_none())
            .and_then(|t| crate::graphql::listening(&t.query));
        Self {
            view,
            strip: Last::new(),
            results: Last::new(),
            heard,
            failed: false,
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        self.failed = false;
        if c.topic(Topic::Search) || c.minute {
            let tabs = Tabs::of(&s.app, self.view.tab()).await;
            self.failed |= tabs.failed;
            events.extend(self.strip.send(&tabs.draw()));
        }
        let answered = c.woke;
        // The tab finishing, or the server saying who is online, redraws
        // what it kept; peers answering redraws what is heard so far.
        if !(answered || c.topic(Topic::Search) || c.topic(Topic::Peers)) {
            return events;
        }
        let tab = match tab(s, &self.view).await {
            Ok(Some(tab)) => tab,
            Ok(None) => return events,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the search tab");
                self.failed = true;
                return events;
            }
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
        events.extend(self.results.send(&results.draw()));
        events
    }

    /// Each time peers answer the running search, until it stops listening.
    fn failed(&self) -> bool {
        self.failed
    }

    fn wakes(&self) -> Option<BoxStream<'static, ()>> {
        let heard = self.heard.clone()?;
        Some(
            futures_util::stream::unfold(heard, |mut heard| async move {
                heard.changed().await.ok()?;
                Some(((), heard))
            })
            .boxed(),
        )
    }
}

/// The tab the address names, as it now stands; none when it names none.
async fn tab(s: &UiState, view: &SearchQuery) -> anyhow::Result<Option<SearchTab>> {
    let Some(id) = view.tab() else {
        return Ok(None);
    };
    Ok(crate::db::search_tab(&s.app.db, id).await?)
}
