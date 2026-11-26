//! A service for the web UI's tests: a real `App` against the in-process
//! test server, signed in, with its state under a directory of the test's own.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slsk_engine::Status;
use slsk_testserver::TestServer;

/// The service as `main` builds it, signed in to `server` as `username`, its
/// library, staging and state under `root`.
pub(super) async fn app(server: &TestServer, root: &Path, username: &str) -> Arc<crate::App> {
    let dirs = ["library", "incomplete", "complete", "state", "share"];
    for d in dirs {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let path = |d: &str| root.join(d).to_string_lossy().into_owned();
    let settings: std::collections::HashMap<&str, String> = [
        (
            "SEAL_KEY",
            "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=".to_string(),
        ),
        ("UI_INSECURE_NO_AUTH", "1".into()),
        ("UI_ADDR", "127.0.0.1:0".into()),
        ("SLSK_SERVER", server.address()),
        ("SLSK_USERNAME", username.into()),
        ("SLSK_PASSWORD", "hunter2".into()),
        ("LIBRARY_DIR", path("library")),
        ("SHARE_DIRS", path("share")),
        ("STAGING_DIR", path("incomplete")),
        ("COMPLETE_DIR", path("complete")),
        ("STATE_DIR", path("state")),
        ("LISTEN_PORT", "0".into()),
        ("MAX_INBOUND_PER_ADDRESS", "100000".into()),
    ]
    .into_iter()
    .collect();
    let cfg = Arc::new(crate::config::Config::from_vars(|k| settings.get(k).cloned()).unwrap());
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
