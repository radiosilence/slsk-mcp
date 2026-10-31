//! End to end over real sockets, against an in-process test server
//! (`slsk-testserver`): peers find each other through it and then speak the
//! peer protocol to each other directly, as on the real network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use slsk_engine::slsk_proto::RawStr;
use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An engine logged in to `server`, with its own state directory under
/// `tmp`.
async fn engine(server: &TestServer, name: &str, shares: Vec<PathBuf>, tmp: &Path) -> Engine {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let mut cfg = EngineConfig::new(name, "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = shares;
    let state = tmp.join(format!("state-{name}"));
    std::fs::create_dir_all(&state).unwrap();
    cfg.state_dir = state;
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

async fn wait_completed(e: &Engine, user: &str, file: &RawStr) {
    for _ in 0..200 {
        let view = e.download_view(user, file).unwrap();
        if view.state == "completed" {
            return;
        }
        assert_ne!(view.state, "failed", "{view:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never completed: {:?}", e.download_view(user, file));
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
    let server = TestServer::start().await;
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
    wait_completed(&bob, "alice", &file.name).await;
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
async fn a_profile_set_at_runtime_is_what_peers_see() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let alice = engine(&server, "alice", vec![], tmp.path()).await;
    let bob = engine(&server, "bob", vec![], tmp.path()).await;
    let picture = bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\nnot really");
    alice.set_profile(slsk_engine::Profile {
        description: "dub techno, mostly".into(),
        picture: Some(picture.clone()),
    });
    let info = tokio::time::timeout(Duration::from_secs(10), bob.user_info("alice"))
        .await
        .expect("no user info")
        .unwrap();
    assert_eq!(info.description, "dub techno, mostly");
    assert_eq!(info.picture, Some(picture));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partial_download_resumes() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let data = album(&music);
    let alice = engine(&server, "alice", vec![music], tmp.path()).await;
    wait_for_shares(&alice, 2).await;
    let bob = engine(&server, "bob", vec![], tmp.path()).await;

    let dest = tmp.path().join("dl/01.flac");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(tmp.path().join("dl/01.flac.part"), &data[..1_000_000]).unwrap();
    let name: RawStr = "music\\Boards of Canada\\Geogaddi\\01 Ready Lets Go.flac".into();
    bob.download("alice", name.clone(), data.len() as u64, dest.clone());
    wait_completed(&bob, "alice", &name).await;
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
async fn an_offer_of_another_size_is_refused() {
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let data = album(&music);
    let alice = engine(&server, "alice", vec![music], tmp.path()).await;
    wait_for_shares(&alice, 2).await;
    let bob = engine(&server, "bob", vec![], tmp.path()).await;

    let dest = tmp.path().join("dl/01.flac");
    let name: RawStr = "music\\Boards of Canada\\Geogaddi\\01 Ready Lets Go.flac".into();
    bob.download("alice", name.clone(), data.len() as u64 + 1, dest.clone());
    for _ in 0..200 {
        let view = bob.download_view("alice", &name).unwrap();
        if view.state == "failed" {
            assert!(view.error.unwrap().contains("bytes"));
            assert!(!dest.exists());
            return;
        }
        assert_ne!(view.state, "completed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the download was not refused");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_login_displaces_the_first() {
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    wait_completed(&alice, "dave", &file.name).await;
    assert_eq!(std::fs::read(&dest).unwrap(), &data[..500_000]);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn searches_past_the_hours_budget_are_refused() {
    let server = TestServer::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new("rationed", "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.state_dir = state.path().to_path_buf();
    cfg.searches_per_hour = 2;
    let engine = Engine::start(cfg).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        engine
            .status()
            .wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .unwrap()
    .unwrap();
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

/// Whether the engine closes `stream` within `within`.
async fn closed_within(stream: &mut tokio::net::TcpStream, within: Duration) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 1024];
    tokio::time::timeout(within, async {
        while let Ok(n) = stream.read(&mut buf).await
            && n > 0
        {}
    })
    .await
    .is_ok()
}

async fn stranger(engine: &Engine) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", engine.listen_port()))
        .await
        .unwrap();
    let init = slsk_proto::peer::PeerInit::PeerInit {
        username: "mallory".into(),
        kind: slsk_proto::ConnKind::Peer,
        token: 0,
    };
    s.write_all(&init.encode()).await.unwrap();
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_announcing_an_oversized_message_is_dropped() {
    use tokio::io::AsyncWriteExt;
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&server, "guarded", Vec::new(), tmp.path()).await;
    // A search response, and a share list nobody asked for, each claiming
    // two megabytes: refused from the header alone.
    for code in [9u32, 5] {
        let mut s = stranger(&engine).await;
        let mut header = (4 + (2u32 << 20)).to_le_bytes().to_vec();
        header.extend_from_slice(&code.to_le_bytes());
        s.write_all(&header).await.unwrap();
        assert!(
            closed_within(&mut s, Duration::from_secs(5)).await,
            "code {code} was buffered"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_connections_are_capped_per_address() {
    let server = TestServer::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new("crowded", "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.state_dir = state.path().to_path_buf();
    cfg.max_inbound_per_address = 2;
    let engine = Engine::start(cfg).await.unwrap();
    let mut held = [stranger(&engine).await, stranger(&engine).await];
    let mut third = stranger(&engine).await;
    assert!(closed_within(&mut third, Duration::from_secs(5)).await);
    assert!(!closed_within(&mut held[0], Duration::from_millis(500)).await);
    drop(held);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut again = stranger(&engine).await;
    assert!(!closed_within(&mut again, Duration::from_millis(500)).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_claiming_a_name_is_not_taken_for_that_user() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = TestServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&server, "asker", Vec::new(), tmp.path()).await;
    // "ghost" is not logged in, so the server places them nowhere and this
    // connection cannot be theirs. Taken at its word, it would receive the
    // request and answer it.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", engine.listen_port()))
        .await
        .unwrap();
    let init = slsk_proto::peer::PeerInit::PeerInit {
        username: "ghost".into(),
        kind: slsk_proto::ConnKind::Peer,
        token: 0,
    };
    s.write_all(&init.encode()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let forger = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
        let forged = slsk_proto::peer::PeerMessage::UserInfoResponse(slsk_proto::peer::UserInfo {
            description: "forged".into(),
            picture: None,
            total_uploads: 0,
            queue_size: 0,
            slots_free: true,
            upload_permitted: None,
        });
        let _ = s.write_all(&forged.encode()).await;
        s
    });
    let info = tokio::time::timeout(Duration::from_secs(20), engine.user_info("ghost"))
        .await
        .unwrap();
    assert!(info.is_err(), "took a stranger's answer: {info:?}");
    drop(forger.await);
}
