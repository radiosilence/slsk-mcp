//! The wishlist: searches repeated on the server's interval until something
//! turns up, and the album each became.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use koan_kit_web as kit;
use uuid::Uuid;

use super::{Draw, Fragment, UiState, failed, one, patch, signals};
use crate::App;
use crate::social::Wish;

pub(super) fn routes() -> Router<UiState> {
    Router::new()
        .route("/wishlist", get(view))
        .route("/wishlist/add", post(add))
        .route("/wishlist/{id}/remove", post(remove))
}

struct Row {
    wish: Wish,
    /// The album it became: title and status, while the job exists.
    job: Option<(String, String)>,
}

#[derive(Template)]
#[template(path = "wishlist.html")]
struct WishlistView {
    rows: Vec<Row>,
    error: Option<String>,
    /// The wish searched next, by its id, and when (ms since the epoch).
    next: Option<(uuid::Uuid, i64)>,
}

impl WishlistView {
    fn searched(&self, w: &Wish) -> String {
        match w.searched_at {
            Some(t) => format!("searched {}", super::ago(t)),
            None => "not searched yet".into(),
        }
    }
    /// When `w` is searched next, if it is the next one.
    fn next_for(&self, w: &Wish) -> Option<i64> {
        let (id, at) = self.next?;
        (id == w.id).then_some(at)
    }
    fn label(&self, status: &str) -> &'static str {
        super::status_label(status)
    }
}

pub(super) async fn html(app: &App) -> Fragment {
    let view = match app.social.wishes().await {
        Ok(wishes) => {
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
            WishlistView {
                rows,
                error: None,
                next,
            }
        }
        Err(e) => WishlistView {
            rows: Vec::new(),
            error: Some(format!("{e:#}")),
            next: None,
        },
    };
    view.draw()
}

#[derive(Template)]
#[template(path = "pages/wishlist.html")]
struct Page {
    list: Fragment,
}

pub(super) async fn page(app: &App) -> Fragment {
    Page {
        list: html(app).await,
    }
    .draw()
}

async fn view(State(s): State<UiState>) -> Response {
    one(html(&s.app).await)
}

#[derive(serde::Deserialize)]
struct AddForm {
    q: String,
    #[serde(default)]
    lossless: bool,
    #[serde(default)]
    grab: bool,
}

async fn add(State(s): State<UiState>, axum::Form(f): axum::Form<AddForm>) -> Response {
    let q = f.q.trim();
    if q.is_empty() {
        return failed(&anyhow::anyhow!("Say what to look for."));
    }
    match s.app.social.add_wish(q, f.lossless, f.grab).await {
        Ok(_) => askama_datastar::events(vec![
            patch(&format!(
                "{}\n{}",
                html(&s.app).await,
                kit::Flash::ok(format!("Wished for {q}"))
            )),
            signals(r#"{"_wishq":""}"#),
        ]),
        Err(e) => failed(&e),
    }
}

async fn remove(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    match s.app.social.remove_wish(id).await {
        Ok(()) => one(format!("{}\n{}", html(&s.app).await, kit::Flash::region())),
        Err(e) => failed(&e),
    }
}
