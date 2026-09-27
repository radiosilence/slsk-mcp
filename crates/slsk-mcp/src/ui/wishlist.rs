//! The wishlist: searches repeated on the server's interval until something
//! turns up, and the album each became.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use uuid::Uuid;

use super::{UiState, failed, flash_ok, many, one, patch, signals};
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
}

impl WishlistView {
    fn sig(&self, w: &Wish) -> String {
        format!("_wish_{}", w.id.simple())
    }
    fn searched(&self, w: &Wish) -> String {
        match w.searched_at {
            Some(t) => format!("searched {}", super::ago(t)),
            None => "not searched yet".into(),
        }
    }
    fn label(&self, status: &str) -> &'static str {
        super::status_label(status)
    }
}

async fn html(app: &App) -> String {
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
            WishlistView { rows, error: None }
        }
        Err(e) => WishlistView {
            rows: Vec::new(),
            error: Some(format!("{e:#}")),
        },
    };
    view.render().unwrap_or_default()
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
        Ok(_) => many(vec![
            patch(&format!(
                "{}\n{}",
                html(&s.app).await,
                flash_ok(&format!("Wished for {q}"))
            )),
            signals(r#"{"_wishq":""}"#),
        ]),
        Err(e) => failed(&e),
    }
}

async fn remove(State(s): State<UiState>, Path(id): Path<Uuid>) -> Response {
    match s.app.social.remove_wish(id).await {
        Ok(()) => one(format!("{}\n<div id=\"flash\"></div>", html(&s.app).await)),
        Err(e) => failed(&e),
    }
}
