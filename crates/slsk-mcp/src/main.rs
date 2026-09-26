//! slsk-mcp — a Soulseek client as a service: the engine, jobs that carry an
//! album from a peer into the library, a web UI, and GraphQL/MCP for an
//! assistant to drive it.

mod analysis;
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
mod social;
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
    pub social: Arc<social::Social>,
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
        cfg.state_dir.join("spectrograms"),
        importer,
    );
    let social = social::Social::new(db.clone(), session.clone(), jobs.clone());
    let app = Arc::new(App {
        cfg: cfg.clone(),
        db,
        session,
        jobs: jobs.clone(),
        social: social.clone(),
    });
    jobs.spawn();
    social.spawn();
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
                        state_metrics(&app, &mut out).await;
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

/// Gauges read from current state at scrape time, where counting as things
/// happen would be a second copy of the truth to keep in step.
async fn state_metrics(app: &App, out: &mut String) {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    let mut gauge = |name: &str, help: &str, rows: &[(String, i64)], label: &str| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
        for (value, n) in rows {
            if label.is_empty() {
                let _ = writeln!(out, "{name} {n}");
            } else {
                let _ = writeln!(out, "{name}{{{label}=\"{value}\"}} {n}");
            }
        }
    };
    if let Some(engine) = app.session.engine() {
        let by_state = |views: Vec<slsk_engine::TransferView>| {
            let mut m: BTreeMap<String, i64> = BTreeMap::new();
            for v in views {
                *m.entry(v.state.to_string()).or_default() += 1;
            }
            m.into_iter().collect::<Vec<_>>()
        };
        gauge(
            "slsk_downloads",
            "Downloads the engine holds, by state.",
            &by_state(engine.downloads()),
            "state",
        );
        gauge(
            "slsk_uploads",
            "Uploads the engine holds, by state (recent history included).",
            &by_state(engine.uploads()),
            "state",
        );
        let (parent, level, _, _) = engine.distributed();
        gauge(
            "slsk_distributed_parent",
            "1 when a distributed-network parent is adopted.",
            &[(String::new(), i64::from(parent.is_some()))],
            "",
        );
        gauge(
            "slsk_distributed_branch_level",
            "Our depth in the distributed search tree; 0 is a branch root.",
            &[(String::new(), i64::from(level))],
            "",
        );
    }
    let jobs: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, count(*) FROM jobs GROUP BY status ORDER BY status")
            .fetch_all(&app.db)
            .await
            .unwrap_or_default();
    gauge(
        "slsk_jobs",
        "Albums on their way into the library, by status.",
        &jobs,
        "status",
    );
    let unread: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE NOT read AND NOT outgoing")
            .fetch_one(&app.db)
            .await
            .unwrap_or(0);
    gauge(
        "slsk_messages_unread",
        "Private messages not yet read.",
        &[(String::new(), unread)],
        "",
    );
    let wishes: i64 = sqlx::query_scalar("SELECT count(*) FROM wishes WHERE job_id IS NULL")
        .fetch_one(&app.db)
        .await
        .unwrap_or(0);
    gauge(
        "slsk_wishes_open",
        "Wishlist searches still looking.",
        &[(String::new(), wishes)],
        "",
    );
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
