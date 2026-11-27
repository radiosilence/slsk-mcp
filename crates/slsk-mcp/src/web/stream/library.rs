//! The Library page live: the list the address describes, and the albums it
//! opens, drawn when the page has none and again when a run over the library
//! ends; and the run panel as the run goes. While a run goes, the panel's
//! head is sent with the albums done since, rather than the panel whole.

use askama_datastar::{ElementPatchMode, Html};
use axum::response::sse::Event;

use super::{Changed, Live, Shown, last};
use crate::live::Topic;
use crate::web::page::Tab;
use crate::web::page::library::run::{RunHead, RunPanel, RunRow};
use crate::web::page::library::{Filter, List, Open, album_html, art_html};
use crate::web::{Draw, UiState, insert, signals};
use askama_datastar::Last;

pub(in crate::web) struct Library {
    filter: Filter,
    open: Vec<String>,
    /// The last run whose changes the page has drawn; 0 before it has a list.
    seen: u64,
    /// The rows the page holds.
    held: usize,
    /// The run the panel last showed, whether it was going, and its albums done.
    panel: Option<(u64, bool, usize)>,
    head: Last,
    whole: Last,
}

impl Live for Library {
    const TAB: Tab = Tab::Library;

    async fn open(_: &UiState, shown: Shown) -> Self {
        Self {
            filter: shown.view(),
            open: shown.view::<Open>().albums(),
            seen: shown.libseen,
            held: shown.librows,
            panel: None,
            head: last(),
            whole: last(),
        }
    }

    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> {
        let mut events = Vec::new();
        if c.topic(Topic::Library) {
            events.extend(self.panel(s));
        }
        let ended = s.library.run.read().seen();
        if self.seen == 0 || ended > self.seen {
            let list = List::of(s, &self.filter, self.held).await;
            // The albums open among the rows drawn: an album open in a row
            // the list no longer holds has nowhere to go.
            let open: Vec<String> = list
                .paths()
                .filter(|p| self.open.iter().any(|o| o == p))
                .map(str::to_string)
                .collect();
            events.push(list.draw().patch().into());
            // A list drawn afresh has nothing ticked.
            events.push(signals(
                serde_json::json!({ "_sel": 0, "_spares": 0, "libseen": ended }),
            ));
            for path in &open {
                events.push(album_html(s, &self.filter, path).await.patch().into());
                events.push(art_html(s, path).await.patch().into());
            }
            self.seen = ended;
        }
        events
    }
}

impl Library {
    /// The run panel's changes since it was last sent.
    fn panel(&mut self, s: &UiState) -> Vec<Event> {
        let run = s.library.run.read();
        let root = s.app.library.root().to_string_lossy().into_owned();
        let done = run.results().len();
        let following = matches!(self.panel, Some((id, true, sent)) if id == run.id() && run.running() && sent <= done);
        let mut events = Vec::new();
        if following {
            let sent = self.panel.map_or(0, |p| p.2);
            events.extend(
                self.head.send(
                    &RunHead {
                        run: &run,
                        root: &root,
                    }
                    .draw(),
                ),
            );
            let new: Html = run.results()[sent..]
                .iter()
                .rev()
                .map(|r| RunRow { r, root: &root }.draw())
                .collect();
            if !new.is_empty() {
                events.push(insert("#lib-run-rows", ElementPatchMode::Prepend, &new));
            }
        } else {
            events.extend(
                self.whole.send(
                    &RunPanel {
                        run: &run,
                        root: &root,
                    }
                    .draw(),
                ),
            );
            self.head = last();
        }
        self.panel = Some((run.id(), run.running(), done));
        events
    }
}
