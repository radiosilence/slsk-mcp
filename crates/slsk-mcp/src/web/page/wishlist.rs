//! The Wishlist tab: searches repeated on the server's interval until
//! something turns up, and the album each became.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Path, State};
use axum::response::Response;
use koan_kit_web as kit;
use uuid::Uuid;

use super::{Body, Page, Tab};
use crate::App;
use crate::social::Wish;
use crate::web::{Draw, UiState, failed, signals};

pub(in crate::web) struct Wishlist;

#[derive(Template)]
#[template(path = "wishlist/page.html")]
struct WishlistPage {
    list: Html,
}

impl Page for Wishlist {
    const TAB: Tab = Tab::Wishlist;
    const STREAM: &'static str = "/stream/wishlist";
    type Args = ();

    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&WishlistPage {
            list: Wishes::of(&s.app).await.draw(),
        }))
    }
}

struct Row {
    wish: Wish,
    /// The album it became: title and status, while the job exists.
    job: Option<(String, String)>,
}

#[derive(Template)]
#[template(path = "wishlist/list.html")]
pub(in crate::web) struct Wishes {
    rows: Vec<Row>,
    error: Option<String>,
    /// The wish searched next, by its id, and when (ms since the epoch).
    next: Option<(Uuid, i64)>,
}

impl Wishes {
    pub(in crate::web) async fn of(app: &App) -> Self {
        let wishes = match app.social.wishes().await {
            Ok(w) => w,
            Err(e) => {
                return Self {
                    rows: Vec::new(),
                    error: Some(format!("{e:#}")),
                    next: None,
                };
            }
        };
        let mut rows = Vec::with_capacity(wishes.len());
        for wish in wishes {
            let job = match wish.job_id {
                Some(id) => crate::db::job(&app.db, id)
                    .await
                    .ok()
                    .flatten()
                    .map(|j| (j.title, j.status)),
                None => None,
            };
            rows.push(Row { wish, job });
        }
        // The wish the loop picks: unfilled, least recently searched.
        let next = rows
            .iter()
            .filter(|r| r.wish.job_id.is_none())
            .min_by_key(|r| r.wish.searched_at)
            .zip(app.social.next_wish())
            .map(|(r, at)| (r.wish.id, at.timestamp_millis()));
        Self {
            rows,
            error: None,
            next,
        }
    }
    fn searched(&self, w: &Wish) -> String {
        match w.searched_at {
            Some(t) => format!("searched {}", crate::web::ago(t)),
            None => "not searched yet".into(),
        }
    }
    /// When `w` is searched next, if it is the next one.
    fn next_for(&self, w: &Wish) -> Option<i64> {
        let (id, at) = self.next?;
        (id == w.id).then_some(at)
    }
    fn label(&self, status: &str) -> &'static str {
        crate::web::status_label(status)
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct AddForm {
    q: String,
    #[serde(default)]
    lossless: bool,
    #[serde(default)]
    grab: bool,
}

/// A wish added; the form is emptied and the stream draws it.
pub(in crate::web) async fn add(
    State(s): State<UiState>,
    axum::Form(f): axum::Form<AddForm>,
) -> Response {
    let q = f.q.trim();
    if q.is_empty() {
        return failed(&anyhow::anyhow!("Say what to look for."));
    }
    match s.app.social.add_wish(q, f.lossless, f.grab).await {
        Ok(_) => askama_datastar::events([
            crate::web::flash::ok(format!("Wished for {q}"))
                .patch()
                .into(),
            signals(serde_json::json!({ "_wishq": "" })),
        ]),
        Err(e) => failed(&e),
    }
}

pub(in crate::web) async fn remove(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    match s.app.social.remove_wish(id).await {
        Ok(()) => askama_datastar::events([crate::web::flash::region().patch().into()]),
        Err(e) => failed(&e),
    }
}
