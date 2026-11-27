//! Every page, in place and whole, every page's stream and the regions the
//! card disclosures read, drawn with markup-shaped text in every field a
//! person or a peer controls: a search, a job's title, a wish, a ban, an
//! interest, a buddy's note, a message, a profile, a peer's name and their
//! shared folders. Each answer must carry that text escaped exactly once,
//! and no markup of the templates' own may come out escaped: that is a
//! fragment spliced in as text, which shows a page of raw HTML.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use askama_datastar::escaping::{Guard, escaped, get};
use axum::http::StatusCode;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::{Engine, EngineConfig, Profile, Status};
use slsk_testserver::TestServer;

use super::UiState;
use crate::folders::{File, Folder};

/// Text shaped like markup, with every character escaping changes.
const HOSTILE: &str = r#"<em>hostile "q" & 'x'</em>"#;
/// A peer named with markup.
const PEER: &str = r#"<b>peer</b>"&'"#;

/// A peer sharing one album, named with markup where the file system
/// allows it: Windows refuses `<`, `>` and `"` in a file name.
async fn peer(server: &TestServer, root: &Path) -> Engine {
    let name = if cfg!(windows) {
        "an album".to_string()
    } else {
        HOSTILE.replace('/', "-")
    };
    let dir = root.join("peer").join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("01 {name}.flac")), b"fLaC").unwrap();
    let mut cfg = EngineConfig::new(PEER, "hunter2");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = vec![root.join("peer")];
    cfg.state_dir = root.join("peer-state");
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
    engine.set_profile(Profile {
        description: HOSTILE.into(),
        picture: None,
    });
    engine
}

/// The peer's album as search results list it.
fn folder() -> Folder {
    let path = format!("{}\\{}", "music", HOSTILE);
    let name = format!("01 {HOSTILE}.flac");
    Folder {
        username: PEER.into(),
        path: path.clone(),
        remote_path: RawStr::from(path.clone()),
        files: vec![File {
            remote: RawStr::from(format!("{path}\\{name}")),
            name,
            extension: "flac".into(),
            size: 4,
            bitrate: None,
            duration: None,
            sample_rate: None,
            bit_depth: None,
        }],
        audio_files: 1,
        total_size: 4,
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

/// A stream's address, with the signals of the page at `url`.
fn stream(path: &str, url: &str) -> String {
    let signals =
        serde_json::json!({ "url": url, "jobs": 100, "history": 100, "files": "", "libseen": 0 });
    super::place_url(path, &[("datastar", &signals.to_string())])
}

#[tokio::test(flavor = "multi_thread")]
async fn markup_from_people_and_peers_is_escaped_once_everywhere() {
    let root = std::env::temp_dir().join(format!("slsk-escaping-{}", uuid::Uuid::new_v4()));
    let server = TestServer::start().await;
    let peer = peer(&server, &root).await;
    let app: Arc<crate::App> = super::fixture::app(&server, &root.join("app"), "listener").await;

    // Everything a person types, or a peer sends, that a page shows.
    let tab = crate::db::open_search_tab(&app.db, HOSTILE, true)
        .await
        .unwrap();
    crate::db::finish_search_tab(&app.db, tab, 1, 1, vec![folder()], None)
        .await
        .unwrap();
    let job = app
        .jobs
        .from_folder(&folder(), Some(HOSTILE.into()), Vec::new())
        .await
        .unwrap();
    app.social.add_wish(HOSTILE, true, false).await.unwrap();
    crate::graphql::set_ban(&app, HOSTILE, true).await.unwrap();
    app.social.set_interest(HOSTILE, Some(true)).await.unwrap();
    app.social.add_buddy(PEER, HOSTILE).await.unwrap();
    let token = app.social.preview("pm", PEER, HOSTILE);
    app.social
        .send_message(&token, PEER, HOSTILE)
        .await
        .unwrap();
    let engine = app.session.engine().unwrap().clone();
    engine.set_profile(Profile {
        description: HOSTILE.into(),
        picture: None,
    });
    // The peer comes by: a browse of our shares, for the Activity page.
    let _ = tokio::time::timeout(Duration::from_secs(10), peer.browse("listener")).await;

    let router = super::routes(UiState::new(app.clone()));
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let job_url = format!("/jobs/{}", job.id);
    let search_url = format!("/search?tab={tab}");
    let library_url = format!("/library?q={}", enc(HOSTILE));
    let browse_url = format!("/browse?username={}", enc(PEER));
    let chat_url = format!("/chat?kind=pm&name={}", enc(PEER));
    let profile_url = format!("/profile?username={}", enc(PEER));
    // Each page, whether its body must show the hostile text, and its stream.
    let pages: Vec<(&str, bool, &str)> = vec![
        ("/", true, "/stream/downloads"),
        (&job_url, true, "/stream/job"),
        (&search_url, true, "/stream/search"),
        ("/uploads", false, "/stream/uploads"),
        ("/activity", false, "/stream/activity"),
        ("/wishlist", true, "/stream/wishlist"),
        (&library_url, true, "/stream/library"),
        (&browse_url, false, "/stream/browse"),
        (&chat_url, true, "/stream/chat"),
        ("/profile", true, "/stream/profile"),
        (&profile_url, false, "/stream/profile"),
        ("/bans", true, "/stream/bans"),
        ("/settings", true, "/stream/settings"),
        ("/triage", false, "/stream/triage"),
    ];
    let guard = Guard::new(&[HOSTILE, PEER])
        .templates(concat!(env!("CARGO_MANIFEST_DIR"), "/templates"))
        .unwrap()
        // The kit's components write these.
        .elements([
            "button", "svg", "path", "label", "input", "dialog", "details", "summary", "progress",
        ]);
    let mut found = Vec::new();
    let mut check = |what: &str, status: StatusCode, text: &str| {
        if !(status.is_success() || status.is_redirection()) {
            found.push(format!("{what}: {status}"));
        }
        found.extend(guard.problems(text).iter().map(|p| format!("{what}: {p}")));
    };
    let mut streamed = String::new();
    let mut missing = Vec::new();
    for (url, shows, live) in &pages {
        for in_place in [false, true] {
            let (status, text) = get(&router, url, in_place, Duration::from_secs(5)).await;
            check(url, status, &text);
            if *shows && !text.contains(&escaped(HOSTILE)) {
                missing.push(format!("{url}: the hostile text is missing"));
            }
        }
        let wait = if live.ends_with("browse") || live.ends_with("profile") {
            6
        } else {
            2
        };
        let (status, text) =
            get(&router, &stream(live, url), true, Duration::from_secs(wait)).await;
        check(live, status, &text);
        streamed.push_str(&text);
    }
    for fragment in [
        format!("/jobs/{}/sources", job.id),
        format!("/jobs/{}/files", job.id),
        format!("/jobs/{}/pick", job.id),
        format!("/chat/rooms?q={}", enc(HOSTILE)),
    ] {
        let (status, text) = get(&router, &fragment, true, Duration::from_secs(5)).await;
        check(&fragment, status, &text);
    }
    // A peer's own words arrive through the streams that draw them.
    for (what, text) in [("the peer's name", PEER), ("their words", HOSTILE)] {
        if !streamed.contains(&escaped(text)) {
            missing.push(format!("no stream drew {what} escaped"));
        }
    }
    found.extend(missing);
    let _ = std::fs::remove_dir_all(&root);
    assert!(found.is_empty(), "{}", found.join("\n"));
}
