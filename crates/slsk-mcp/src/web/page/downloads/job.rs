//! A job's own page, under the Downloads tab: its card while it exists,
//! then where its files come from, what has happened to it, and the log of
//! its import.

use askama::Template;
use axum::extract::Path;
use axum::http::StatusCode;
use uuid::Uuid;

use super::card::Card;
use super::{Sources, Tab};
use crate::App;
use crate::web::page::{Body, Page};
use crate::web::{Draw, UiState};

pub(in crate::web) struct Job;

#[derive(Template)]
#[template(path = "downloads/job.html")]
pub(in crate::web) struct JobPage {
    card: Option<Card>,
    sources: Sources,
    import_log: Option<String>,
    /// Neither the job nor any history of it is known.
    gone: bool,
}

impl JobPage {
    /// The job's page, with what to call it.
    pub(in crate::web) async fn of(app: &App, id: Uuid) -> anyhow::Result<(Self, String)> {
        let (job, events) = tokio::join!(
            crate::db::job(&app.db, id),
            crate::db::job_events(&app.db, id)
        );
        let (job, events) = (job?, events?);
        let gone = job.is_none() && events.is_empty();
        let sources = Sources::new(job.as_ref(), events);
        let Some(job) = job else {
            let title = sources.title();
            return Ok((
                Self {
                    card: None,
                    sources,
                    import_log: None,
                    gone,
                },
                title,
            ));
        };
        let view = crate::graphql::job_view(app, job, true)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        let import_log = view.import_log.clone().filter(|l| !l.trim().is_empty());
        let card = Card::of(app, view, true, true);
        let title = card.heading();
        Ok((
            Self {
                card: Some(card),
                sources,
                import_log,
                gone,
            },
            title,
        ))
    }
}

impl Page for Job {
    const TAB: Tab = Tab::Downloads;
    const STREAM: &'static str = "/stream/job";
    type Args = Path<Uuid>;

    async fn body(s: &UiState, Path(id): Path<Uuid>) -> anyhow::Result<Body> {
        let (page, title) = JobPage::of(&s.app, id).await?;
        let status = if page.gone {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        Ok(Body::of(&page).heading(title).status(status))
    }
}

/// The job a job page's address names.
pub(in crate::web) fn of_path(path: &str) -> Option<Uuid> {
    path.strip_prefix("/jobs/")
        .and_then(|id| Uuid::parse_str(id).ok())
}
