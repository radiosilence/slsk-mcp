//! The Browse page live: a peer's shares, asked of them when no listing is
//! kept. A peer sends its whole share list at once and takes a while, so the
//! page draws a note first and the tree once they answer.

use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use tokio::task::JoinHandle;

use super::{Changed, Last, Live, Shown};
use crate::web::page::Tab;
use crate::web::page::browse::{BrowseQuery, Info, Listing, Place, Shares};
use crate::web::{Draw, UiState};

/// How long a peer is given to send their profile beside their shares.
const INFO_WAIT: Duration = Duration::from_secs(20);

pub(in crate::web) struct Browse {
    place: Option<Place>,
    /// The peer's answer, while it is awaited.
    asking: Option<JoinHandle<Result<(Listing, Option<Info>), String>>>,
    /// The answer, once it came, to draw.
    answer: Option<Result<(Listing, Option<Info>), String>>,
    shares: Last,
    /// Something new to draw: the first time, and once the peer answers.
    due: bool,
}

impl Live for Browse {
    const TAB: Tab = Tab::Browse;

    async fn open(s: &UiState, shown: Shown) -> Self {
        let place = Place::from_query(&shown.view::<BrowseQuery>()).ok();
        let engine = s.app.session.engine().cloned();
        let asking = match (&place, engine) {
            (Some(p), Some(engine))
                if engine.username() != p.username && s.browsed.get(&p.username).is_none() =>
            {
                let (cache, username) = (s.browsed.clone(), p.username.clone());
                Some(tokio::spawn(async move {
                    let info = tokio::time::timeout(INFO_WAIT, engine.user_info(&username));
                    let (dirs, info) = tokio::join!(engine.browse(&username), info);
                    let listing: Listing = Arc::new(dirs.map_err(|e| e.to_string())?);
                    cache.put(&username, listing.clone());
                    let info = info.ok().and_then(Result::ok).map(|i| Info {
                        description: i.description,
                        uploads: i.total_uploads,
                        queue: i.queue_size,
                        free: i.slots_free,
                    });
                    Ok((listing, info))
                }))
            }
            _ => None,
        };
        Self {
            place,
            asking,
            answer: None,
            shares: Last::default(),
            due: true,
        }
    }

    async fn draw(&mut self, s: &UiState, _: &Changed) -> Vec<Event> {
        let (Some(p), Some(engine), true) = (&self.place, s.app.session.engine(), self.due) else {
            return Vec::new();
        };
        self.due = false;
        let mine = engine.username() == p.username;
        let banned = engine.is_banned(&p.username);
        let shares = match &self.answer {
            Some(Ok((listing, info))) => Shares::tree(p, listing, info.clone(), false, banned),
            Some(Err(e)) => Shares::note(
                &p.username,
                format!("{} did not send their shares: {e}", p.username),
                false,
            ),
            None if self.asking.is_some() => Shares::note(
                &p.username,
                format!("Asking {} for their shares…", p.username),
                true,
            ),
            None if mine => Shares::tree(p, &s.browsed.own(engine, false), None, true, false),
            None => match s.browsed.get(&p.username) {
                Some(l) => Shares::tree(p, &l, None, false, banned),
                None => return Vec::new(),
            },
        };
        self.shares.send(shares.draw()).into_iter().collect()
    }

    async fn wake(&mut self) {
        let Some(asking) = self.asking.as_mut() else {
            return std::future::pending().await;
        };
        self.answer = Some(asking.await.unwrap_or_else(|e| Err(e.to_string())));
        self.asking = None;
        self.due = true;
    }
}
