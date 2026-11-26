//! The Search page live: the tab strip as searches open, finish and close;
//! the tab shown, redrawn as peers answer while its search runs; and whether
//! the peers behind its results are online, asked of the server once they are
//! drawn, since kept results can be hours old.

use std::collections::HashMap;
use std::time::Duration;

use axum::response::sse::Event;
use slsk_engine::Event as EngineEvent;
use slsk_engine::slsk_proto::server::{FromServer, ToServer, UserStatus};
use tokio::sync::{broadcast, watch};

use super::{Changed, Last, Live, Shown};
use crate::db::SearchTab;
use crate::graphql::Heard;
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::search::{LISTEN, Results, SearchQuery, Tabs, folder_id};
use crate::web::{Draw, UiState};

/// How long the server is given to say who is online.
const STATUS_WAIT: Duration = Duration::from_secs(5);

pub(in crate::web) struct Search {
    view: SearchQuery,
    tab: Option<SearchTab>,
    strip: Last,
    results: Last,
    /// The running search shown, while it runs.
    heard: Option<watch::Receiver<Heard>>,
    /// Peers have answered since the results were last drawn.
    answered: bool,
    /// Whether the peers behind the results have been asked about.
    asked: bool,
    /// The peers asked about and the folders of each, until they answer.
    asking: Option<Asking>,
    /// Answers in hand, to send with the next draw.
    statuses: Vec<(u64, &'static str)>,
}

struct Asking {
    events: broadcast::Receiver<EngineEvent>,
    folders: HashMap<String, Vec<u64>>,
    until: tokio::time::Instant,
}

impl Live for Search {
    const TAB: Tab = Tab::Search;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let view: SearchQuery = shown.view();
        let tab = match view.tab() {
            Some(id) => crate::db::search_tab(&s.app.db, id).await.ok().flatten(),
            None => None,
        };
        let heard = tab
            .as_ref()
            .filter(|t| t.searched_at.is_none())
            .zip(s.app.session.engine())
            .map(|(t, e)| crate::graphql::listen(e, &t.query, LISTEN));
        Self {
            view,
            tab,
            strip: Last::default(),
            results: Last::default(),
            heard,
            answered: false,
            asked: false,
            asking: None,
            statuses: Vec::new(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.topic(Topic::Search) || c.minute {
            let tabs = Tabs::of(&s.app, self.view.tab()).await;
            events.extend(self.strip.send(tabs.draw()));
        }
        if let (Some(heard), Some(tab), Some(engine), true) = (
            self.heard.as_mut(),
            &self.tab,
            s.app.session.engine(),
            std::mem::take(&mut self.answered),
        ) {
            let now = heard.borrow_and_update().clone();
            let results = Results::heard(engine, tab, &now, self.view.shown());
            events.extend(self.results.send(results.draw()));
            // Done, or its search went away: drawn as it ended, and not waited on again.
            if now.done || heard.has_changed().is_err() {
                self.heard = None;
                self.ask(s, &results);
            }
        } else if !self.asked
            && let Some(tab) = self.tab.as_ref().filter(|t| t.searched_at.is_some())
        {
            // The page drew the kept results; only who is online is left.
            let results = Results::kept(&s.app, tab, self.view.shown()).await;
            self.ask(s, &results);
        }
        events.extend(self.statuses.drain(..).map(|(id, word)| {
            crate::web::state(word, word)
                .id(format!("peer-{id}"))
                .html()
                .patch()
                .into()
        }));
        events
    }

    async fn wake(&mut self) {
        if let Some(heard) = self.heard.as_mut() {
            let _ = heard.changed().await;
            self.answered = true;
            return;
        }
        let Some(asking) = self.asking.as_mut() else {
            return std::future::pending().await;
        };
        loop {
            let msg = match tokio::time::timeout_at(asking.until, asking.events.recv()).await {
                Ok(Ok(EngineEvent::Server(msg))) => msg,
                Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) | Err(_) => {
                    self.asking = None;
                    return;
                }
            };
            let FromServer::UserStatus {
                username, status, ..
            } = &*msg
            else {
                continue;
            };
            let Some(ids) = asking.folders.remove(username) else {
                continue;
            };
            let word = match status {
                UserStatus::Online => "online",
                UserStatus::Away => "away",
                UserStatus::Offline => "offline",
            };
            self.statuses.extend(ids.into_iter().map(|id| (id, word)));
            if asking.folders.is_empty() {
                self.asking = None;
            }
            return;
        }
    }
}

impl Search {
    /// Ask the server whether each peer behind `results` is online.
    fn ask(&mut self, s: &UiState, results: &Results) {
        self.asked = true;
        let Some(engine) = s.app.session.engine() else {
            return;
        };
        let mut folders: HashMap<String, Vec<u64>> = HashMap::new();
        for f in &results.folders {
            folders
                .entry(f.username.clone())
                .or_default()
                .push(folder_id(f));
        }
        let events = engine.events();
        for username in folders.keys() {
            if engine
                .send(ToServer::GetUserStatus {
                    username: username.clone(),
                })
                .is_err()
            {
                return;
            }
        }
        self.asking = (!folders.is_empty()).then(|| Asking {
            events,
            folders,
            until: tokio::time::Instant::now() + STATUS_WAIT,
        });
    }
}
