//! Jobs against real peers: the in-process test server, a sharing peer and
//! the service's own engine, over loopback sockets.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

use crate::config::{Config, PeerLimits};
use crate::folders::Folder;
use crate::jobs::Jobs;

const ALBUMS: usize = 5;
const FILES: usize = 2;
const FILE_BYTES: usize = 256 << 10;

async fn logged_in(engine: &Engine) {
    let mut status = engine.status();
    tokio::time::timeout(
        Duration::from_secs(30),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .expect("logged in")
    .unwrap();
}

/// A peer sharing `ALBUMS` albums as `sharer\Album N`, sending slowly
/// enough that an album takes a few seconds.
async fn sharer(server: &TestServer, root: &Path) -> Engine {
    for a in 0..ALBUMS {
        let dir = root.join("sharer").join(format!("Album {a}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..FILES {
            let data: Vec<u8> = format!("{a}/{f}:")
                .bytes()
                .cycle()
                .take(FILE_BYTES)
                .collect();
            std::fs::write(dir.join(format!("{f:02}.flac")), data).unwrap();
        }
    }
    let mut cfg = EngineConfig::new("sharer", "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = vec![root.join("sharer")];
    cfg.state_dir = root.join("sharer-state");
    cfg.upload_limit = 256 << 10;
    cfg.max_inbound_per_address = usize::MAX;
    std::fs::create_dir_all(&cfg.state_dir).unwrap();
    let engine = Engine::start(cfg).await.unwrap();
    logged_in(&engine).await;
    while engine.share_counts().1 < ALBUMS * FILES {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    engine
}

fn config(server: &TestServer, root: &Path, peer_limits: PeerLimits) -> Config {
    let dir = |d: &str| {
        let p = root.join(d);
        std::fs::create_dir_all(&p).unwrap();
        p
    };
    Config {
        seal_key: [7; 32],
        account: Some(("listener".into(), "hunter2".into())),
        server: server.address(),
        library_dir: dir("library"),
        share_dirs: vec![dir("share")],
        staging_dir: dir("incomplete"),
        complete_dir: dir("complete"),
        state_dir: dir("state"),
        listen_port: 0,
        upload_slots: 2,
        upload_limit: 0,
        download_limit: 0,
        searches_per_hour: 0,
        max_inbound_connections: 512,
        max_inbound_per_address: usize::MAX,
        internal_addr: "127.0.0.1:0".into(),
        metrics_addr: "127.0.0.1:0".into(),
        ui_addr: "127.0.0.1:0".into(),
        public_url: "http://127.0.0.1".into(),
        oidc: None,
        beets_config: None,
        discogs_token: None,
        acoustid_client: None,
        description: String::new(),
        optional_extensions: Vec::new(),
        peer_limits,
    }
}

async fn jobs(cfg: Config, root: &Path) -> Arc<Jobs> {
    let cfg = Arc::new(cfg);
    let db = crate::db::Db::open(&root.join("state/slsk.db"))
        .await
        .unwrap();
    let importer = Arc::new(sift::Importer::new(sift::Config {
        directory: cfg.library_dir.clone(),
        ..Default::default()
    }));
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
    logged_in(session.engine().unwrap()).await;
    let jobs = Jobs::new(db, session, &cfg, importer, library);
    jobs.spawn();
    jobs
}

fn album(n: usize) -> Folder {
    let path = format!("sharer\\Album {n}");
    Folder {
        username: "sharer".into(),
        remote_path: path.clone().into(),
        path,
        files: Vec::new(),
        audio_files: 0,
        total_size: 0,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_is_asked_for_no_more_jobs_at_once_than_the_limit() {
    let root = std::env::temp_dir().join(format!("slsk-peer-limit-{}", uuid::Uuid::new_v4()));
    let server = TestServer::start().await;
    let _sharer = sharer(&server, &root).await;
    let limits = PeerLimits {
        jobs: 2,
        ..PeerLimits::default()
    };
    let jobs = jobs(config(&server, &root, limits), &root).await;
    let engine = jobs.session.engine().unwrap().clone();

    let mut ids = Vec::new();
    for a in 0..ALBUMS {
        ids.push(
            jobs.from_folder(&album(a), None, Vec::new())
                .await
                .unwrap()
                .id,
        );
    }
    // The same folder again is the same job.
    let again = jobs.from_folder(&album(0), None, Vec::new()).await.unwrap();
    assert_eq!(again.id, ids[0]);

    let album_of = |filename: &str| -> Option<usize> {
        filename
            .strip_prefix("sharer\\Album ")?
            .split('\\')
            .next()?
            .parse()
            .ok()
    };
    let mut asked = std::collections::HashSet::new();
    let mut most = 0;
    let mut saw_held = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while asked.len() < ALBUMS {
        assert!(
            tokio::time::Instant::now() < deadline,
            "asked for {asked:?} of {ALBUMS} albums"
        );
        let at_once: std::collections::HashSet<usize> = engine
            .downloads()
            .iter()
            .filter(|d| !matches!(d.state, "completed" | "failed" | "cancelled"))
            .filter_map(|d| album_of(&d.filename.to_string_lossy()))
            .collect();
        most = most.max(at_once.len());
        asked.extend(at_once);
        saw_held |= ids.iter().any(|id| jobs.held(*id).is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(most <= 2, "{most} albums were asked of the peer at once");
    assert!(saw_held, "albums past the limit waited here");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_banned_peer_is_never_a_source() {
    let root = std::env::temp_dir().join(format!("slsk-banned-{}", uuid::Uuid::new_v4()));
    let server = TestServer::start().await;
    let _sharer = sharer(&server, &root).await;
    let jobs = jobs(config(&server, &root, PeerLimits::default()), &root).await;
    let engine = jobs.session.engine().unwrap().clone();

    let job = jobs.from_folder(&album(0), None, Vec::new()).await.unwrap();
    // As `graphql::set_ban` does: through the database, which a login's own
    // push of the bans reads too, so one landing after this keeps the ban.
    let account = engine.username();
    crate::db::set_ban(&jobs.db, &account, "Sharer", true)
        .await
        .unwrap();
    jobs.session.apply_bans(&engine, &account).await.unwrap();

    let refused = jobs.from_folder(&album(1), None, Vec::new()).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("banned")),
        "a banned peer's folder became a job: {refused:?}"
    );
    let heard = crate::graphql::responses(&engine, "sharer album", 3)
        .await
        .unwrap();
    assert!(
        heard.is_empty(),
        "a search was answered after the ban: {:?}",
        heard
            .iter()
            .map(|r| (&r.username, r.files.len()))
            .collect::<Vec<_>>()
    );

    // Nobody else has the album, so the job is left failed, with the reason.
    let moved = jobs.leave_peer("sharer", job.id).await.unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].to, None);
    assert!(moved[0].why.is_some());
    let job = crate::db::job(&jobs.db, job.id).await.unwrap().unwrap();
    assert_eq!(job.status, "failed");
    assert!(
        engine
            .downloads()
            .iter()
            .all(|d| d.state == "cancelled" || !d.username.eq_ignore_ascii_case("sharer")),
        "a transfer from the banned peer is still going"
    );
    let _ = std::fs::remove_dir_all(&root);
}
