//! A Soulseek network on this machine for working on the web UI: the test
//! server on a fixed address and one peer sharing a folder, so searches
//! answer and downloads run. `just dev` starts it beside the service.
//!
//! cargo run -p slsk-mcp --example dev_network -- <address> <share dir> <state dir>

use std::path::PathBuf;
use std::time::Duration;

use slsk_engine::{Engine, EngineConfig, Status};
use slsk_testserver::TestServer;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(share), Some(state)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: dev_network <address> <share dir> <state dir>");
        std::process::exit(2);
    };
    let server = TestServer::start_on(&addr).await;

    let mut cfg = EngineConfig::new("dev-sharer", "dev");
    cfg.server = server.address();
    cfg.listen_port = 0;
    cfg.share_dirs = vec![PathBuf::from(share)];
    cfg.state_dir = PathBuf::from(state);
    // Slow enough that a download shows its progress for a while.
    cfg.upload_limit = 512 << 10;
    cfg.max_inbound_per_address = usize::MAX;
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let engine = Engine::start(cfg).await.expect("start the sharing peer");
    let mut status = engine.status();
    tokio::time::timeout(
        Duration::from_secs(30),
        status.wait_for(|s| matches!(s, Status::LoggedIn { .. })),
    )
    .await
    .expect("the sharing peer logs in")
    .expect("status");
    println!("dev network on {}: dev-sharer is sharing", server.address());

    tokio::signal::ctrl_c().await.ok();
    drop(engine);
}
