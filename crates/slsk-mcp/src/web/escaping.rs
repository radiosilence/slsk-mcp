//! Every page, in place and whole, every page's stream and the regions the
//! card disclosures read, drawn with markup-shaped text in every field a
//! person or a peer controls: a search, a job's title, a wish, a ban, an
//! interest, a buddy's note, a message, a profile, a peer's name and their
//! shared folders. Each answer must carry that text escaped exactly once,
//! and no markup of the templates' own may come out escaped: that is a
//! fragment spliced in as text, which shows a page of raw HTML.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use futures_util::StreamExt;
use slsk_engine::slsk_proto::RawStr;
use slsk_engine::{Engine, EngineConfig, Profile, Status};
use slsk_testserver::TestServer;
use tower::ServiceExt;

use super::UiState;
use crate::folders::{File, Folder};

/// Text shaped like markup, with every character escaping changes.
const HOSTILE: &str = r#"<em>hostile "q" & 'x'</em>"#;
/// How it reads once escaped, as far as is needed to find it.
const ESCAPED: &str = "&#60;em&#62;hostile &#34;q&#34; &#38; &#39;x&#39;&#60;/em&#62;";
/// A peer named with markup.
const PEER: &str = r#"<b>peer</b>"&'"#;
const PEER_ESCAPED: &str = "&#60;b&#62;peer&#60;/b&#62;&#34;&#38;&#39;";

/// Every element name the templates write, from the templates themselves.
fn tags() -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for part in text.split('<').skip(1) {
                let name: String = part
                    .trim_start_matches('/')
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect();
                if !name.is_empty() {
                    tags.insert(name.to_ascii_lowercase());
                }
            }
        }
    }
    // The kit's components write these.
    tags.extend(
        [
            "button", "svg", "path", "label", "input", "dialog", "details", "summary", "progress",
        ]
        .map(String::from),
    );
    tags
}

/// The answer's problems: template markup escaped, the hostile text escaped
/// twice, or left as markup where the answer is HTML.
fn problems(what: &str, text: &str, tags: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for (at, open) in text
        .match_indices("&#60;")
        .chain(text.match_indices("&lt;"))
    {
        let rest = text[at + open.len()..].trim_start_matches('/');
        let name: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        // The hostile text's own elements are meant to come out escaped.
        if tags.contains(&name) && name != "em" && name != "b" {
            let end = (at + 80).min(text.len());
            let end = (end..=text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len());
            out.push(format!("{what}: escaped markup: {}", &text[at..end]));
        }
    }
    for twice in [
        "&#38;#60;",
        "&#38;#34;",
        "&#38;#39;",
        "&#38;#38;",
        "&amp;lt;",
    ] {
        if text.contains(twice) {
            out.push(format!("{what}: escaped twice ({twice})"));
        }
    }
    // Signals are JSON, not markup, so the text is raw there by right.
    let markup: String = text
        .lines()
        .filter(|l| !l.starts_with("data: signals"))
        .collect::<Vec<_>>()
        .join("\n");
    for raw in ["<em>hostile", "<b>peer</b>"] {
        if markup.contains(raw) {
            out.push(format!("{what}: not escaped ({raw})"));
        }
    }
    out
}

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

/// What the router answers to a GET of `uri`, read for at most `wait`: a
/// stream is cut off there, having sent what it draws first.
async fn get(router: &axum::Router, uri: &str, datastar: bool, wait: Duration) -> String {
    let mut request = Request::get(uri);
    if datastar {
        request = request.header("datastar-request", "true");
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        response.status().is_success() || response.status().is_redirection(),
        "{uri}: {}",
        response.status()
    );
    let mut body = response.into_body().into_data_stream();
    let mut text = Vec::new();
    let ends = tokio::time::Instant::now() + wait;
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(ends, body.next()).await {
        text.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&text).into_owned()
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
    let tags = tags();
    let mut found = Vec::new();
    let mut streamed = String::new();
    for (url, shows, live) in &pages {
        for datastar in [false, true] {
            let text = get(&router, url, datastar, Duration::from_secs(5)).await;
            found.extend(problems(url, &text, &tags));
            if *shows && !text.contains(ESCAPED) {
                found.push(format!("{url}: the hostile text is missing"));
            }
        }
        let wait = if live.ends_with("browse") || live.ends_with("profile") {
            6
        } else {
            2
        };
        let text = get(&router, &stream(live, url), true, Duration::from_secs(wait)).await;
        found.extend(problems(live, &text, &tags));
        streamed.push_str(&text);
    }
    for fragment in [
        format!("/jobs/{}/sources", job.id),
        format!("/jobs/{}/files", job.id),
        format!("/jobs/{}/pick", job.id),
        format!("/chat/rooms?q={}", enc(HOSTILE)),
    ] {
        let text = get(&router, &fragment, true, Duration::from_secs(5)).await;
        found.extend(problems(&fragment, &text, &tags));
    }
    // A peer's own words arrive through the streams that ask them.
    for (what, text) in [("the peer's name", PEER_ESCAPED), ("their words", ESCAPED)] {
        if !streamed.contains(text) {
            found.push(format!("no stream drew {what} escaped"));
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(found.is_empty(), "{}", found.join("\n"));
}
