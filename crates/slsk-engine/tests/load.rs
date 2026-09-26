//! Transfers at a scale no other client is asked to handle: tens of thousands
//! queued at once, through real sockets to the in-process test server.
//!
//! Ignored by default; run with
//! `cargo test --release -p slsk-engine --test load -- --ignored --nocapture`.
//! `LOAD_PEERS` and `LOAD_FILES` (per peer) set the scale, default 40 × 1000.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

fn scale() -> (usize, usize) {
    let var = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    (var("LOAD_PEERS", 40), var("LOAD_FILES", 1000))
}

async fn engine(
    server: &TestServer,
    name: &str,
    shares: Vec<PathBuf>,
    state: &Path,
    tune: impl FnOnce(&mut EngineConfig),
) -> Engine {
    let mut cfg = EngineConfig::new(name, "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = shares;
    cfg.state_dir = state.join(name);
    std::fs::create_dir_all(&cfg.state_dir).unwrap();
    // A download client asking for a thousand files from one peer is the
    // point here, not abuse to be refused.
    cfg.max_queued_files_per_user = 1_000_000;
    cfg.max_queued_bytes_per_user = u64::MAX;
    tune(&mut cfg);
    let engine = Engine::start(cfg).await.unwrap();
    let mut status = engine.status();
    tokio::time::timeout(
        Duration::from_secs(30),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .unwrap_or_else(|_| panic!("{name} did not log in"))
    .unwrap();
    engine
}

/// `n` small files under `root/<name>`, each with content only it has, so a
/// file delivered to the wrong place or cut short cannot pass for another.
fn share(root: &Path, name: &str, n: usize) -> Vec<(String, Vec<u8>)> {
    let dir = root.join(name).join("Album");
    std::fs::create_dir_all(&dir).unwrap();
    (0..n)
        .map(|i| {
            let file = format!("{i:05} {name} track.flac");
            let data: Vec<u8> = format!("{name}/{i}:")
                .bytes()
                .cycle()
                .take(4096 + i % 997)
                .collect();
            std::fs::write(dir.join(&file), &data).unwrap();
            (format!("{name}\\Album\\{file}"), data)
        })
        .collect()
}

async fn wait_for_shares(e: &Engine, files: usize) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while e.share_counts().1 < files {
        assert!(Instant::now() < deadline, "shares never scanned");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits for every download to finish, reporting progress, and returns how
/// long it took. Fails on the first failed transfer.
async fn drain(downloader: &Engine, expected: usize, label: &str) -> Duration {
    let started = Instant::now();
    let mut last = Instant::now();
    loop {
        let views = downloader.downloads();
        let done = views.iter().filter(|v| v.state == "completed").count();
        if let Some(v) = views.iter().find(|v| v.state == "failed") {
            panic!("{label}: a transfer failed after {done} completed: {v:?}");
        }
        if done >= expected {
            return started.elapsed();
        }
        if last.elapsed() > Duration::from_secs(5) {
            let active = views.iter().filter(|v| v.state == "transferring").count();
            eprintln!(
                "{label}: {done}/{expected} done, {active} transferring, {:?}",
                started.elapsed()
            );
            last = Instant::now();
        }
        assert!(
            started.elapsed() < Duration::from_secs(1800),
            "{label}: stalled at {done}/{expected}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn verify(files: &[(PathBuf, Vec<u8>)]) {
    for (path, data) in files {
        let got = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            got == *data,
            "{} arrived with the wrong bytes",
            path.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test; run explicitly"]
async fn one_client_downloads_from_every_peer_at_once() {
    let (peers, per) = scale();
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();

    let mut sharers = Vec::new();
    let mut wanted = Vec::new();
    for p in 0..peers {
        let name = format!("peer{p:03}");
        let files = share(&tmp.path().join("shares"), &name, per);
        let e = engine(
            &server,
            &name,
            vec![tmp.path().join("shares").join(&name)],
            tmp.path(),
            |c| {
                c.upload_slots = 10;
            },
        )
        .await;
        wait_for_shares(&e, per).await;
        wanted.push((name, files));
        sharers.push(e);
    }
    let hoarder = engine(&server, "hoarder", vec![], tmp.path(), |_| {}).await;

    let mut expect = Vec::new();
    let queued = Instant::now();
    for (name, files) in &wanted {
        for (i, (remote, data)) in files.iter().enumerate() {
            let dest = tmp
                .path()
                .join("got")
                .join(name)
                .join(format!("{i:05}.flac"));
            hoarder.download(
                name,
                remote.as_str().into(),
                data.len() as u64,
                dest.clone(),
            );
            expect.push((dest, data.clone()));
        }
    }
    eprintln!(
        "queued {} downloads in {:?}",
        expect.len(),
        queued.elapsed()
    );

    let took = drain(&hoarder, expect.len(), "downloads").await;
    let bytes: usize = expect.iter().map(|(_, d)| d.len()).sum();
    eprintln!(
        "{} downloads from {peers} peers in {took:?}: {:.0} files/s, {:.1} MB/s",
        expect.len(),
        expect.len() as f64 / took.as_secs_f64(),
        bytes as f64 / took.as_secs_f64() / 1e6
    );
    verify(&expect);
    drop(sharers);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test; run explicitly"]
async fn one_library_serves_every_client_at_once() {
    let (clients, per) = scale();
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();

    let files = share(&tmp.path().join("shares"), "hub", per);
    let hub = engine(
        &server,
        "hub",
        vec![tmp.path().join("shares").join("hub")],
        tmp.path(),
        |c| {
            c.upload_slots = 50;
        },
    )
    .await;
    wait_for_shares(&hub, per).await;

    let mut downloaders = Vec::new();
    let mut expect = Vec::new();
    for c in 0..clients {
        let name = format!("client{c:03}");
        let e = engine(&server, &name, vec![], tmp.path(), |_| {}).await;
        for (i, (remote, data)) in files.iter().enumerate() {
            let dest = tmp
                .path()
                .join("got")
                .join(&name)
                .join(format!("{i:05}.flac"));
            e.download(
                "hub",
                remote.as_str().into(),
                data.len() as u64,
                dest.clone(),
            );
            expect.push((dest, data.clone()));
        }
        downloaders.push(e);
    }
    eprintln!("{} uploads requested of one engine", expect.len());

    let started = Instant::now();
    // The transfers run in the background either way; waiting on each in
    // turn takes as long as the slowest.
    for (c, e) in downloaders.iter().enumerate() {
        drain(e, per, &format!("client{c:03}")).await;
    }
    let took = started.elapsed();
    // The hub's transfer list keeps recent history only; its counters
    // cover everything.
    let completed = hub
        .metrics()
        .uploads_completed
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let failed = hub
        .metrics()
        .uploads_failed
        .load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "{} uploads from one engine in {took:?}: {:.0} files/s; hub counts {completed} completed, {failed} failed",
        expect.len(),
        expect.len() as f64 / took.as_secs_f64()
    );
    assert_eq!(completed, expect.len());
    assert_eq!(failed, 0);
    verify(&expect);
}
