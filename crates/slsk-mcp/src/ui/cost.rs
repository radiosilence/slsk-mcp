//! What the live stream costs with a realistic page: fifteen albums
//! downloading at once, each from its own peer, through the in-process test
//! server and real sockets, while the Downloads page's stream is read.
//!
//! Ignored by default; run with
//! `cargo test --release -p slsk-mcp live_stream_cost -- --ignored --nocapture`.
//! `COST_SECS` sets each measuring window, default 30.
//!
//! It reports the stream's bytes a second as sent before compression and
//! after the brotli the service applies (flushed after every event, as
//! `compress_events` does), the events a second, and the process's CPU over
//! a window with the stream open against one without it. The process also
//! runs the test server and the fifteen sharing peers, so the CPU figure
//! that matters is the difference.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

use super::{Shown, StreamQuery, UiState, stream};
use crate::folders::{File, Folder};

const JOBS: usize = 15;
const FILES: usize = 6;
const FILE_BYTES: usize = 6 << 20;
/// Each peer's upload limit, so every album takes minutes and stays in
/// progress through both windows.
const PEER_RATE: u64 = 96 << 10;

fn cpu() -> Duration {
    // SAFETY: getrusage writes the struct it is given and nothing else.
    let usage = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        u
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

async fn peer(server: &TestServer, name: &str, root: &Path) -> Engine {
    let dir = root.join(name).join("Album");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..FILES {
        let data: Vec<u8> = format!("{name}/{i}:")
            .bytes()
            .cycle()
            .take(FILE_BYTES)
            .collect();
        std::fs::write(dir.join(format!("{i:02} {name}.flac")), data).unwrap();
    }
    let mut cfg = EngineConfig::new(name, "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = vec![root.join(name)];
    cfg.state_dir = root.join(format!("{name}-state"));
    cfg.upload_limit = PEER_RATE;
    cfg.max_inbound_per_address = usize::MAX;
    std::fs::create_dir_all(&cfg.state_dir).unwrap();
    let engine = Engine::start(cfg).await.unwrap();
    let mut status = engine.status();
    tokio::time::timeout(
        Duration::from_secs(30),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .expect("peer logged in")
    .unwrap();
    while engine.share_counts().1 < FILES {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    engine
}

fn folder(name: &str) -> Folder {
    let files = (0..FILES)
        .map(|i| {
            let file = format!("{i:02} {name}.flac");
            File {
                remote: RawStr::from(format!("{name}\\Album\\{file}")),
                name: file,
                size: FILE_BYTES as u64,
                extension: "flac".into(),
                bitrate: None,
                duration: None,
                sample_rate: None,
                bit_depth: None,
            }
        })
        .collect();
    Folder {
        username: name.into(),
        path: format!("{name}\\Album"),
        remote_path: RawStr::from(format!("{name}\\Album")),
        files,
        audio_files: FILES,
        total_size: (FILES * FILE_BYTES) as u64,
        lossless: true,
        bit_depth: None,
        sample_rate: None,
        bitrate: None,
        free_slot: true,
        speed: 0,
        queue_length: 0,
        score: 0.0,
    }
}

async fn app(server: &TestServer, root: &Path) -> Arc<crate::App> {
    let dirs = ["library", "incomplete", "complete", "state", "share"];
    for d in dirs {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let path = |d: &str| root.join(d).to_string_lossy().into_owned();
    // SAFETY: set before anything else in this test process reads them, and
    // this test runs alone (`--ignored`, by name).
    unsafe {
        for (k, v) in [
            (
                "SEAL_KEY",
                "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=".to_string(),
            ),
            ("UI_INSECURE_NO_AUTH", "1".into()),
            ("UI_ADDR", "127.0.0.1:0".into()),
            ("SLSK_SERVER", server.address()),
            ("SLSK_USERNAME", "listener".into()),
            ("SLSK_PASSWORD", "hunter2".into()),
            ("LIBRARY_DIR", path("library")),
            ("SHARE_DIRS", path("share")),
            ("STAGING_DIR", path("incomplete")),
            ("COMPLETE_DIR", path("complete")),
            ("STATE_DIR", path("state")),
            ("LISTEN_PORT", "0".into()),
            ("MAX_INBOUND_PER_ADDRESS", "100000".into()),
        ] {
            std::env::set_var(k, v);
        }
    }
    let cfg = Arc::new(crate::config::Config::from_env().unwrap());
    let db = crate::db::Db::open(&root.join("state/slsk.db"))
        .await
        .unwrap();
    let sift_cfg = sift::Config {
        directory: root.join("library"),
        ..Default::default()
    };
    let importer = Arc::new(sift::Importer::new(sift_cfg));
    let library = Arc::new(
        crate::library::Library::open(
            &root.join("state/library.db"),
            importer.clone(),
            root.join("library-bin"),
        )
        .unwrap(),
    );
    let session = crate::session::Session::new(cfg.clone(), db.clone());
    session.boot().await.unwrap();
    let mut status = session.engine().unwrap().status();
    tokio::time::timeout(
        Duration::from_secs(30),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .expect("listener logged in")
    .unwrap();
    let jobs = crate::jobs::Jobs::new(db.clone(), session.clone(), &cfg, importer, library.clone());
    let social = crate::social::Social::new(db.clone(), session.clone(), jobs.clone());
    let db_for_activity = db.clone();
    jobs.spawn();
    Arc::new(crate::App {
        cfg,
        db,
        session,
        jobs,
        social,
        library,
        activity: crate::activity::Activity::new(db_for_activity),
    })
}

/// Reads the Downloads page's stream for `secs`: raw bytes, brotli bytes
/// and events.
async fn read_stream(state: UiState, window: Duration) -> (u64, u64, u64) {
    let response = stream(
        State(state),
        Query(StreamQuery {
            page: "albums".into(),
        }),
        askama_datastar::Signals(Shown {
            jobs: 100,
            files: String::new(),
            ..Shown::default()
        }),
    )
    .await
    .into_response();
    let mut body = response.into_body().into_data_stream();
    let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
    let (mut raw, mut br, mut events) = (0u64, 0u64, 0u64);
    let ends = tokio::time::Instant::now() + window;
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(ends, body.next()).await {
        raw += chunk.len() as u64;
        events += chunk.windows(6).filter(|w| w == b"event:").count() as u64;
        encoder.write_all(&chunk).unwrap();
        encoder.flush().unwrap();
        br += std::mem::take(encoder.get_mut()).len() as u64;
    }
    (raw, br, events)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures the live stream's cost; run explicitly"]
async fn live_stream_cost() {
    let secs: u64 = std::env::var("COST_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let window = Duration::from_secs(secs);
    let root: PathBuf = std::env::temp_dir().join(format!("slsk-cost-{}", uuid::Uuid::new_v4()));
    let server = TestServer::start().await;
    let mut peers = Vec::new();
    for i in 0..JOBS {
        peers.push(peer(&server, &format!("peer{i:02}"), &root.join("peers")).await);
    }
    let app = app(&server, &root).await;
    for i in 0..JOBS {
        app.jobs
            .from_folder(&folder(&format!("peer{i:02}")), None, Vec::new())
            .await
            .unwrap();
    }
    // Every album moving before anything is measured.
    let engine = app.session.engine().unwrap().clone();
    let started = Instant::now();
    loop {
        let moving = engine
            .downloads()
            .iter()
            .filter(|d| d.state == "transferring")
            .map(|d| d.username.clone())
            .collect::<std::collections::HashSet<_>>()
            .len();
        if moving == JOBS {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "only {moving} albums moving"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let state = UiState {
        config: app.cfg.clone(),
        sessions: crate::auth::session::Sessions::new(app.db.clone()),
        app: app.clone(),
        http: reqwest::Client::new(),
        run: Arc::new(crate::live::Watched::new(
            crate::live::Topic::Library,
            Default::default(),
        )),
        slot: Default::default(),
        art: Default::default(),
        browsed: Default::default(),
        profiles: Default::default(),
    };

    let before = cpu();
    tokio::time::sleep(window).await;
    let idle = cpu() - before;

    let before = cpu();
    let (raw, br, events) = read_stream(state, window).await;
    let busy = cpu() - before;

    let downloading = engine
        .downloads()
        .iter()
        .filter(|d| d.state == "transferring")
        .count();
    let per_sec = |n: u64| n as f64 / window.as_secs_f64();
    let pct = |d: Duration| d.as_secs_f64() / window.as_secs_f64() * 100.0;
    eprintln!("{JOBS} albums, {downloading} files transferring at the end, {secs}s windows");
    eprintln!(
        "stream: {:.0} B/s before compression, {:.0} B/s brotli, {:.1} events/s",
        per_sec(raw),
        per_sec(br),
        per_sec(events)
    );
    eprintln!(
        "process CPU: {:.1}% of a core without the stream, {:.1}% with it: the stream costs {:.1}%",
        pct(idle),
        pct(busy),
        pct(busy) - pct(idle)
    );
    let _ = std::fs::remove_dir_all(&root);
}
