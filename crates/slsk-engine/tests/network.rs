//! End to end over real sockets, against an in-process test server
//! (`slsk-testserver`): peers find each other through it and then speak the
//! peer protocol to each other directly, as on the real network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn server() -> Option<TestServer> {
    Some(TestServer::start().await)
}

async fn engine(server: &TestServer, name: &str, shares: Vec<PathBuf>, state: &Path) -> Engine {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let mut cfg = EngineConfig::new(name, "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = shares;
    cfg.state_dir = state.to_path_buf();
    let engine = Engine::start(cfg).await.unwrap();
    let mut status = engine.status();
    tokio::time::timeout(
        Duration::from_secs(10),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .unwrap_or_else(|_| panic!("{name} did not log in: {:?}", engine.status().borrow()))
    .unwrap();
    engine
}

fn album(root: &Path) -> Vec<u8> {
    let dir = root.join("Boards of Canada").join("Geogaddi");
    std::fs::create_dir_all(&dir).unwrap();
    let data: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::write(dir.join("01 Ready Lets Go.flac"), &data).unwrap();
    std::fs::write(dir.join("02 Music Is Math.flac"), &data[..1000]).unwrap();
    data
}

async fn wait_for_shares(e: &Engine, files: usize) {
    for _ in 0..100 {
        if e.share_counts().1 >= files {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("shares never scanned");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_engines_search_and_download() {
    let Some(server) = server().await else { return };
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let data = album(&music);

    let alice = engine(&server, "alice", vec![music.clone()], tmp.path()).await;
    wait_for_shares(&alice, 2).await;
    let bob = engine(&server, "bob", vec![], tmp.path()).await;

    let mut results = bob.search("geogaddi ready").unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), results.recv())
        .await
        .expect("no search result")
        .unwrap();
    assert_eq!(response.username, "alice");
    assert_eq!(response.files.len(), 1);
    let file = &response.files[0];
    assert_eq!(
        file.name.to_string_lossy(),
        "music\\Boards of Canada\\Geogaddi\\01 Ready Lets Go.flac"
    );

    let dest = tmp.path().join("downloads/01.flac");
    bob.download("alice", file.name.clone(), file.size, dest.clone());
    for _ in 0..200 {
        let view = bob.download_view("alice", &file.name).unwrap();
        if view.state == "completed" {
            break;
        }
        assert_ne!(view.state, "failed", "{view:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let uploads = alice.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].state, "completed");

    let listing = bob.browse("alice").await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].files.len(), 2);

    let folder = bob
        .folder_contents("alice", &"music\\Boards of Canada".into())
        .await
        .unwrap();
    assert_eq!(
        folder.len(),
        1,
        "the album folder sits under the artist folder"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partial_download_resumes() {
    let Some(server) = server().await else { return };
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let data = album(&music);
    let alice = engine(&server, "alice", vec![music], tmp.path()).await;
    wait_for_shares(&alice, 2).await;
    let bob = engine(&server, "bob", vec![], tmp.path()).await;

    let dest = tmp.path().join("dl/01.flac");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(tmp.path().join("dl/01.flac.part"), &data[..1_000_000]).unwrap();
    let name: slsk_engine::slsk_proto::RawStr =
        "music\\Boards of Canada\\Geogaddi\\01 Ready Lets Go.flac".into();
    bob.download("alice", name.clone(), data.len() as u64, dest.clone());
    for _ in 0..200 {
        if bob.download_view("alice", &name).unwrap().state == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let sent = alice
        .metrics()
        .bytes_uploaded
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        sent,
        data.len() as u64 - 1_000_000,
        "only the missing tail is sent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_login_displaces_the_first() {
    let Some(server) = server().await else { return };
    let tmp = tempfile::tempdir().unwrap();
    let first = engine(&server, "carol", vec![], tmp.path()).await;
    let _second = engine(&server, "carol", vec![], tmp.path()).await;
    let mut status = first.status();
    tokio::time::timeout(
        Duration::from_secs(10),
        status.wait_for(|s| *s == Status::Displaced),
    )
    .await
    .unwrap()
    .unwrap();
}

/// The reference client from soulseek-rs, as an independent implementation:
/// it must find and download our files, and we must download theirs.
#[tokio::test(flavor = "multi_thread")]
async fn interoperates_with_soulseek_rs() {
    let Some(server) = server().await else { return };
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let data = album(&music);
    let alice = engine(&server, "alice", vec![music.clone()], tmp.path()).await;
    wait_for_shares(&alice, 2).await;

    let port = server.addr.port();
    let their_share = tmp.path().join("theirs");
    std::fs::create_dir_all(&their_share).unwrap();
    std::fs::write(
        their_share.join("Autechre - Gantz Graf.flac"),
        &data[..500_000],
    )
    .unwrap();
    let their_dl = tmp.path().join("their-downloads");
    std::fs::create_dir_all(&their_dl).unwrap();
    let listen = free_port();
    let (share, dl) = (their_share.clone(), their_dl.clone());
    let client = tokio::task::spawn_blocking(move || {
        use soulseek_rs::{Client, ClientSettings, PeerAddress};
        let mut settings = ClientSettings::new("dave", "hunter2");
        settings.server_address = PeerAddress::new("127.0.0.1".into(), port);
        settings.enable_listen = true;
        settings.listen_port = listen;
        settings.shared_directories = vec![share.display().to_string()];
        let mut c = Client::with_settings(settings);
        c.connect().unwrap();
        assert!(c.login().unwrap());
        let results = c.search("geogaddi ready", Duration::from_secs(5)).unwrap();
        let file = results
            .iter()
            .flat_map(|r| r.files.iter())
            .find(|f| f.username == "alice")
            .expect("alice's file")
            .clone();
        let (_d, rx) = c
            .download(
                file.name.clone(),
                file.username.clone(),
                file.size,
                dl.display().to_string(),
            )
            .unwrap();
        for status in rx.iter() {
            if status.is_terminal() {
                assert!(
                    matches!(status, soulseek_rs::DownloadStatus::Completed),
                    "{status:?}"
                );
                break;
            }
        }
        c
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(their_dl.join("01 Ready Lets Go.flac")).unwrap(),
        data
    );

    let mut results = alice.search("gantz graf").unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), results.recv())
        .await
        .expect("no result from dave")
        .unwrap();
    let file = response.files[0].clone();
    let dest = tmp.path().join("ours/gantz.flac");
    alice.download("dave", file.name.clone(), file.size, dest.clone());
    for _ in 0..200 {
        let v = alice.download_view("dave", &file.name).unwrap();
        if v.state == "completed" {
            break;
        }
        assert_ne!(v.state, "failed", "{v:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(&dest).unwrap(), &data[..500_000]);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn searches_past_the_hours_budget_are_refused() {
    let server = server().await.unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new("rationed", "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.state_dir = state.path().to_path_buf();
    cfg.searches_per_hour = 2;
    let engine = Engine::start(cfg).await.unwrap();
    engine.pace().await.unwrap();
    engine.pace().await.unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(1), engine.pace())
        .await
        .expect("a spent budget refuses at once rather than waiting");
    assert!(matches!(
        refused,
        Err(slsk_engine::Error::SearchBudget { budget: 2, .. })
    ));
}
