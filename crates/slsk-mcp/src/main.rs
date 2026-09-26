//! slsk-mcp — a Soulseek client as a service: the engine, jobs that carry an
//! album from a peer into the library, a web UI, and GraphQL/MCP for an
//! assistant to drive it.

mod auth;
mod config;
mod crypto;
mod db;
mod error;
mod folders;
mod graphql;
mod jobs;
mod mcp;
mod session;
mod ui;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};

use crate::config::Config;
use crate::session::Session;

pub struct App {
    pub cfg: Arc<Config>,
    pub db: sqlx::PgPool,
    pub session: Arc<Session>,
    pub jobs: Arc<jobs::Jobs>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if std::env::args().nth(1).as_deref() == Some("schema") {
        println!("{}", graphql::sdl());
        return Ok(());
    }

    let cfg = Arc::new(Config::from_env()?);
    let db = db::connect(&cfg.database_url).await.context("database")?;
    for dir in [&cfg.staging_dir, &cfg.complete_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("creating {}", cfg.state_dir.display()))?;

    let mut sift_cfg = match &cfg.beets_config {
        Some(p) => {
            sift::Config::load(p).with_context(|| format!("beets config {}", p.display()))?
        }
        None => sift::Config::default(),
    };
    sift_cfg.directory = cfg.library_dir.clone();
    // Staging is ours; moving out of it is the whole point.
    sift_cfg.move_files = true;
    sift_cfg.musicbrainz_contact = "https://github.com/radiosilence/slsk-mcp".into();
    let importer = Arc::new(sift::Importer::new(sift_cfg));

    let session = Session::new(cfg.clone(), db.clone());
    session.boot().await?;
    let jobs = jobs::Jobs::new(
        db.clone(),
        session.clone(),
        cfg.staging_dir.clone(),
        cfg.complete_dir.clone(),
        importer,
    );
    let app = Arc::new(App {
        cfg: cfg.clone(),
        db,
        session,
        jobs: jobs.clone(),
    });
    jobs.spawn();
    if app.session.engine().is_some()
        && let Err(e) = jobs.resume().await
    {
        tracing::warn!(error = %e, "could not resume jobs");
    }

    let internal = internal_router(app.clone());
    let internal_listener = tokio::net::TcpListener::bind(&cfg.internal_addr).await?;
    tracing::info!(addr = %cfg.internal_addr, "internal listener: /mcp /graphql /metrics");
    let ui_listener = tokio::net::TcpListener::bind(&cfg.ui_addr).await?;
    tracing::info!(addr = %cfg.ui_addr, "web UI");
    let ui = ui::router(app.clone());
    tokio::try_join!(
        async {
            axum::serve(internal_listener, internal)
                .with_graceful_shutdown(shutdown())
                .await
        },
        async {
            axum::serve(ui_listener, ui)
                .with_graceful_shutdown(shutdown())
                .await
        },
    )?;
    Ok(())
}

/// MCP, GraphQL and metrics. Credentials in headers are trusted here, so this
/// listener is reachable from the gateway and the metrics scraper and nothing
/// else — the deployment's NetworkPolicy is what enforces that.
fn internal_router(app: Arc<App>) -> Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    };
    let template = mcp::SlskMcp::new(app.clone());
    let service = StreamableHttpService::new(
        move || Ok(template.clone()),
        Arc::new(LocalSessionManager::default()),
        // Behind a trusted proxy that forwards an internal Host header, which
        // rmcp's DNS-rebinding allowlist would reject.
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    );
    let schema = graphql::schema(app.clone());
    Router::new()
        .nest_service("/mcp", service)
        .route(
            "/graphql",
            post({
                let app = app.clone();
                move |headers: http::HeaderMap,
                      axum::Json(req): axum::Json<async_graphql::Request>| {
                    let (app, schema) = (app.clone(), schema.clone());
                    async move {
                        if let Err(e) = mcp::apply_credentials(&app, &headers).await {
                            return axum::Json(async_graphql::Response::from_errors(vec![
                                async_graphql::ServerError::new(e, None),
                            ]));
                        }
                        axum::Json(schema.execute(req).await)
                    }
                }
            }),
        )
        .route(
            "/metrics",
            get({
                let app = app.clone();
                move || {
                    let app = app.clone();
                    async move {
                        let mut out = String::new();
                        if let Some(engine) = app.session.engine() {
                            engine.metrics().render(&mut out);
                        }
                        (
                            [(http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                            out,
                        )
                    }
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
}

/// Kubernetes stops a pod with SIGTERM; a terminal with Ctrl-C.
async fn shutdown() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}
