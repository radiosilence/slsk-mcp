//! The Bans tab: who gets nothing from us, and banning by name.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Query, State};
use axum::response::Response;
use koan_kit_web as kit;

use super::{Body, Page, Tab};
use crate::App;
use crate::web::peer::PeerQuery;
use crate::web::{Draw, UiState, failed, field, fields, signals};

pub(in crate::web) struct Bans;

#[derive(Template)]
#[template(path = "bans/page.html")]
struct BansPage {
    list: Html,
}

impl Page for Bans {
    const TAB: Tab = Tab::Bans;
    const STREAM: &'static str = "/stream/bans";
    type Args = ();

    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&BansPage {
            list: List::of(&s.app).await.draw(),
        }))
    }
}

#[derive(Template)]
#[template(path = "bans/list.html")]
struct List {
    bans: Vec<String>,
    error: Option<String>,
}

impl List {
    async fn of(app: &App) -> Self {
        let result = async {
            let engine = app.session.require()?;
            anyhow::Ok(crate::db::bans(&app.db, &engine.username()).await?)
        }
        .await;
        match result {
            Ok(bans) => Self { bans, error: None },
            Err(e) => Self {
                bans: Vec::new(),
                error: Some(format!("{e:#}")),
            },
        }
    }
}

async fn set(s: &UiState, user: &str, banned: bool) -> Response {
    let user = user.trim();
    if user.is_empty() {
        return failed(&anyhow::anyhow!("Say who."));
    }
    if let Err(e) = crate::graphql::set_ban(&s.app, user, banned).await {
        return failed(&anyhow::anyhow!("{}", e.message));
    }
    let said = if banned {
        format!("Banned {user}")
    } else {
        format!("Unbanned {user}")
    };
    askama_datastar::events([
        List::of(&s.app).await.draw().patch().into(),
        crate::web::flash::ok(&said).patch().into(),
        signals(serde_json::json!({ "_banuser": "" })),
        kit::Confirm::close().into(),
    ])
}

/// The name typed on the page, posted by its form once the page's question is answered.
pub(in crate::web) async fn ban(State(s): State<UiState>, body: axum::body::Bytes) -> Response {
    let form = fields(&body);
    set(&s, field(&form, "username").unwrap_or_default(), true).await
}

pub(in crate::web) async fn unban(
    State(s): State<UiState>,
    Query(p): Query<PeerQuery>,
) -> Response {
    set(&s, &p.username, false).await
}
