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
mod library;
mod mcp;
mod pg_import;
mod session;
mod social;
mod ui;
mod uploads_log;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};

use crate::config::Config;
use crate::session::Session;

pub struct App {
    pub cfg: Arc<Config>,
    pub db: crate::db::Db,
    pub session: Arc<Session>,
    pub jobs: Arc<jobs::Jobs>,
    pub social: Arc<social::Social>,
    pub library: Arc<library::Library>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match std::env::args().nth(1).as_deref() {
        Some("schema") => {
            println!("{}", graphql::sdl());
            return Ok(());
        }
        // The transcode check the importer runs, on any files, one JSON line
        // each.
        Some("analyse") => {
            for path in std::env::args().skip(2) {
                match analysis::analyse(std::path::Path::new(&path), None) {
                    Ok(a) => println!("{}", serde_json::to_string(&a)?),
                    Err(e) => eprintln!("{path}: {e:#}"),
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let cfg = Arc::new(Config::from_env()?);
    // The library must already be there. On a removable drive an absent
    // library means the drive is not mounted, and everything written from
    // here on would land on whatever disk holds the mountpoint instead.
    anyhow::ensure!(
        cfg.library_dir.is_dir(),
        "library {} does not exist; is the drive mounted?",
        cfg.library_dir.display()
    );
    for dir in [&cfg.staging_dir, &cfg.complete_dir] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("creating {}", cfg.state_dir.display()))?;
    let db = db::Db::open(&cfg.state_dir.join("slsk.db"))
        .await
        .context("database")?;
    if let Some(url) = &cfg.import_from {
        // History is worth keeping, not worth staying down for: the account
        // also comes from the environment.
        if let Err(e) = pg_import::run(&db, url).await {
            tracing::error!(
                error = format!("{e:#}"),
                "importing from Postgres failed; starting without its history"
            );
        }
    }
    {
        let db = db.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            every.tick().await;
            loop {
                every.tick().await;
                db.optimize().await;
            }
        });
    }

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
    // The root filesystem is read-only in a container; the state volume is
    // where a cache survives restarts.
    sift_cfg.cache_dir = Some(cfg.state_dir.join("cache"));
    let importer = Arc::new(sift::Importer::new(sift_cfg));
    // Spare copies go beside the library rather than in it: out of
    // Navidrome's and the shares' sight, and on the same drive, so binning
    // is a rename.
    let bin = {
        let mut name = cfg
            .library_dir
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        name.push("-bin");
        cfg.library_dir.with_file_name(name)
    };
    let library = Arc::new(library::Library::open(
        &cfg.state_dir.join("library.db"),
        importer.clone(),
        bin,
    )?);

    let session = Session::new(cfg.clone(), db.clone());
    session.boot().await?;
    let jobs = jobs::Jobs::new(
        db.clone(),
        session.clone(),
        cfg.staging_dir.clone(),
        cfg.complete_dir.clone(),
        cfg.state_dir.join("spectrograms"),
        importer,
        library.clone(),
    );
    let social = social::Social::new(db.clone(), session.clone(), jobs.clone());
    let app = Arc::new(App {
        cfg: cfg.clone(),
        db,
        session,
        jobs: jobs.clone(),
        social: social.clone(),
        library: library.clone(),
    });
    tokio::spawn(async move { library.follow().await });
    jobs.spawn();
    social.spawn();
    uploads_log::spawn(app.clone());
    if app.session.engine().is_some()
        && let Err(e) = jobs.resume().await
    {
        tracing::warn!(error = %e, "could not resume jobs");
    }

    let internal = internal_router(app.clone());
    let internal_listener = tokio::net::TcpListener::bind(&cfg.internal_addr).await?;
    tracing::info!(addr = %cfg.internal_addr, "internal listener: /mcp /graphql");
    let metrics_listener = tokio::net::TcpListener::bind(&cfg.metrics_addr).await?;
    tracing::info!(addr = %cfg.metrics_addr, "metrics: /metrics");
    let metrics = metrics_router(app.clone());
    let ui_listener = tokio::net::TcpListener::bind(&cfg.ui_addr).await?;
    tracing::info!(addr = %cfg.ui_addr, "web UI");
    let ui = ui::router(app.clone());
    // On a stop signal no new import starts, but the listeners keep serving
    // until the one in progress has finished: the UI and MCP stay up through
    // a drain that can take minutes, rather than going dark for it.
    let (stopped, _) = tokio::sync::watch::channel(false);
    let stop = stopped.clone();
    tokio::spawn(async move {
        shutdown().await;
        tracing::info!("stopping; waiting for any import in progress");
        jobs.drain().await;
        let _ = stop.send(true);
    });
    let when_drained = || {
        let mut rx = stopped.subscribe();
        async move {
            let _ = rx.wait_for(|done| *done).await;
        }
    };
    tokio::try_join!(
        async {
            axum::serve(internal_listener, internal)
                .with_graceful_shutdown(when_drained())
                .await
        },
        async {
            axum::serve(ui_listener, ui)
                .with_graceful_shutdown(when_drained())
                .await
        },
        async {
            axum::serve(metrics_listener, metrics)
                .with_graceful_shutdown(when_drained())
                .await
        },
    )?;
    Ok(())
}

/// MCP and GraphQL. Credentials in headers are trusted here, so this listener
/// is reachable from the gateway and nothing else — the deployment's
/// NetworkPolicy is what enforces that.
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
        .route("/healthz", get(|| async { "ok" }))
}

/// Metrics alone, on a listener of their own, so a scraper can be let in
/// without being let near the port that believes credentials in headers.
fn metrics_router(app: Arc<App>) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
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
        }),
    )
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
            .fetch_all(&app.db.read)
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
            .fetch_one(&app.db.read)
            .await
            .unwrap_or(0);
    gauge(
        "slsk_messages_unread",
        "Private messages not yet read.",
        &[(String::new(), unread)],
        "",
    );
    let wishes: i64 = sqlx::query_scalar("SELECT count(*) FROM wishes WHERE job_id IS NULL")
        .fetch_one(&app.db.read)
        .await
        .unwrap_or(0);
    gauge(
        "slsk_wishes_open",
        "Wishlist searches still looking.",
        &[(String::new(), wishes)],
        "",
    );
    let (day, week, ever) = db::served_user_counts(&app.db).await.unwrap_or_default();
    gauge(
        "slsk_served_users",
        "Distinct users an upload has finished to, by how recently.",
        &[
            ("24h".to_string(), day),
            ("7d".to_string(), week),
            ("all".to_string(), ever),
        ],
        "window",
    );
    // Kept in the database, so a deploy does not reset them.
    let totals = db::totals(&app.db).await.unwrap_or_default();
    let total = |name: &str| {
        totals
            .iter()
            .find(|(n, _)| n == name)
            .map_or(0, |(_, v)| *v)
    };
    let _ = writeln!(
        out,
        "# HELP slsk_lifetime_uploaded_bytes_total Bytes sent to other users, ever.\n# TYPE slsk_lifetime_uploaded_bytes_total counter\nslsk_lifetime_uploaded_bytes_total {}",
        total("uploaded_bytes")
    );
    let _ = writeln!(
        out,
        "# HELP slsk_lifetime_downloaded_bytes_total Bytes of files downloaded, ever.\n# TYPE slsk_lifetime_downloaded_bytes_total counter\nslsk_lifetime_downloaded_bytes_total {}",
        total("downloaded_bytes")
    );
    let _ = writeln!(
        out,
        "# HELP slsk_lifetime_uploads_total Uploads that ended, ever, by how.\n# TYPE slsk_lifetime_uploads_total counter"
    );
    for (name, n) in &totals {
        if let Some(state) = name.strip_prefix("uploads_") {
            let _ = writeln!(out, "slsk_lifetime_uploads_total{{state=\"{state}\"}} {n}");
        }
    }
    // The history only grows, so its counts are counters: rate() over them
    // is how often each thing goes wrong.
    let _ = writeln!(
        out,
        "# HELP slsk_job_outcomes_total Outcomes jobs reached, by outcome and why.\n# TYPE slsk_job_outcomes_total counter"
    );
    for (outcome, cause, n) in db::event_counts(&app.db).await.unwrap_or_default() {
        let _ = writeln!(
            out,
            "slsk_job_outcomes_total{{outcome=\"{outcome}\",cause=\"{}\"}} {n}",
            cause.as_deref().unwrap_or("")
        );
    }
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
