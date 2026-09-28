//! The server session: log in, keep it alive, dispatch what arrives, and
//! reconnect when it drops — unless the server said to stop.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use slsk_proto::CodeWidth;
use slsk_proto::server::{FromServer, ToServer, UserStatus};
use tokio::net::lookup_host;
use tokio::sync::mpsc;

use crate::net::{self, Conn};
use crate::{Event, Inner, Status, distributed, peers};

/// Server frames are small; the largest is a room list.
const MAX_FRAME: usize = 16 << 20;

pub(crate) async fn run(inner: Arc<Inner>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let banned = *inner.ban_until.lock();
        if let Some(until) = banned.filter(|u| *u > tokio::time::Instant::now()) {
            inner.set_status(Status::Disconnected {
                error: "banned by the server; waiting for it to lift".into(),
            });
            tokio::time::sleep_until(until).await;
            backoff = Duration::from_secs(2);
        }
        inner.set_status(Status::Connecting);
        let outcome = session(&inner).await;
        inner.server_tx.write().take();
        distributed::reset(&inner, false);
        let status = match outcome {
            Ok(()) => Status::Disconnected {
                error: "connection closed".into(),
            },
            Err(End::Displaced) => Status::Displaced,
            Err(End::Rejected(reason)) => Status::Rejected { reason },
            Err(End::Io(e)) => Status::Disconnected { error: e },
            Err(End::Reconnect) => {
                backoff = Duration::from_secs(2);
                continue;
            }
        };
        let wait_for_operator = matches!(status, Status::Displaced | Status::Rejected { .. });
        tracing::warn!(?status, "server session ended");
        inner.set_status(status);
        if wait_for_operator {
            inner.reconnect.notified().await;
            backoff = Duration::from_secs(2);
        } else {
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = inner.reconnect.notified() => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(120));
        }
    }
}

enum End {
    Displaced,
    Rejected(String),
    Io(String),
    Reconnect,
}

async fn session(inner: &Arc<Inner>) -> Result<(), End> {
    let io = |e: std::io::Error| End::Io(e.to_string());
    let addr = lookup_host(&inner.cfg.server)
        .await
        .map_err(io)?
        .next()
        .ok_or_else(|| End::Io("server address did not resolve".into()))?;
    let mut conn = Conn::new(
        net::connect(addr, Duration::from_secs(20))
            .await
            .map_err(io)?,
    );
    let (username, password) = {
        let c = inner.credentials.read();
        (c.0.clone(), c.1.to_string())
    };
    use tokio::io::AsyncWriteExt;
    conn.stream
        .write_all(
            &ToServer::Login {
                username: username.clone(),
                password,
            }
            .encode(),
        )
        .await
        .map_err(io)?;
    let first = tokio::time::timeout(
        Duration::from_secs(30),
        conn.frame(CodeWidth::U32, MAX_FRAME),
    )
    .await
    .map_err(|_| End::Io("no login response".into()))?
    .map_err(io)?
    .ok_or_else(|| End::Io("server closed during login".into()))?;
    let own_ip = match FromServer::decode(first.code, first.body) {
        Ok(FromServer::LoginOk {
            own_ip, greeting, ..
        }) => {
            tracing::info!(%username, %own_ip, greeting, "logged in");
            own_ip
        }
        Ok(FromServer::LoginRejected { reason, detail }) => {
            return Err(End::Rejected(
                detail.map_or(reason.clone(), |d| format!("{reason}: {d}")),
            ));
        }
        other => return Err(End::Io(format!("unexpected login response: {other:?}"))),
    };

    let (mut reader, writer) = conn.split();
    let (tx, rx) = mpsc::channel(4096);
    net::spawn_writer(writer, rx);
    *inner.server_tx.write() = Some(tx.clone());

    let shares = inner.shares.load();
    for msg in [
        ToServer::SetWaitPort {
            port: u32::from(inner.cfg.listen_port),
        },
        ToServer::SharedFoldersFiles {
            dirs: shares.dir_count() as u32,
            files: shares.file_count() as u32,
        },
        ToServer::SetStatus {
            status: UserStatus::Online,
        },
        ToServer::CheckPrivileges,
        ToServer::AcceptChildren {
            accept: inner.cfg.accept_children,
        },
    ] {
        let _ = tx.send(msg.encode()).await;
    }
    distributed::reset(inner, true);
    inner.set_status(Status::LoggedIn { own_ip });

    let keepalive = {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                if tx.send(ToServer::ServerPing.encode()).await.is_err() {
                    break;
                }
            }
        })
    };

    let result = loop {
        let frame = tokio::select! {
            f = reader.frame(CodeWidth::U32, MAX_FRAME) => f,
            _ = inner.reconnect.notified() => break Err(End::Reconnect),
        };
        let frame = match frame {
            Ok(Some(f)) => f,
            Ok(None) => break Ok(()),
            Err(e) => break Err(End::Io(e.to_string())),
        };
        let msg = match FromServer::decode(frame.code, frame.body) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(code = frame.code, error = %e, "undecodable server message");
                continue;
            }
        };
        if matches!(msg, FromServer::Relogged) {
            break Err(End::Displaced);
        }
        dispatch(inner, &msg);
        let _ = inner.events.send(Event::Server(Arc::new(msg)));
    };
    keepalive.abort();
    result
}

fn dispatch(inner: &Arc<Inner>, msg: &FromServer) {
    match msg {
        FromServer::PeerAddress {
            username, ip, port, ..
        } => peers::on_address(inner, username, *ip, *port),
        FromServer::ConnectToPeer {
            username,
            kind,
            ip,
            port,
            token,
            ..
        } => {
            tokio::spawn(peers::answer_indirect(
                inner.clone(),
                username.clone(),
                *kind,
                *ip,
                *port,
                *token,
            ));
        }
        FromServer::CantConnectToPeer { token } => peers::on_cant_connect(inner, *token),
        FromServer::FileSearch {
            username,
            token,
            query,
        } => peers::respond_to_search(inner, username, *token, query),
        FromServer::MessageUser {
            id,
            username,
            message,
            ..
        } => {
            let _ = inner.send_server(ToServer::MessageAcked { id: *id });
            if username == "server"
                && let Some(minutes) = ban_minutes(message)
            {
                // A minute over, so the first login lands after the ban.
                let wait = Duration::from_secs((minutes + 1) * 60);
                *inner.ban_until.lock() = Some(tokio::time::Instant::now() + wait);
                tracing::warn!(
                    minutes,
                    "banned by the server; not logging in until it lifts"
                );
            }
        }
        FromServer::PossibleParents { parents } => distributed::on_possible_parents(inner, parents),
        FromServer::EmbeddedMessage { code: 3, payload } => {
            distributed::on_root_search(inner, payload.clone())
        }
        FromServer::ResetDistributed => distributed::reset(inner, true),
        FromServer::ParentSpeedRatio { ratio } => inner.dist.set_ratio(*ratio),
        FromServer::ExcludedSearchPhrases { phrases } => *inner.excluded.write() = phrases.clone(),
        FromServer::PrivilegedUsers { usernames } => {
            *inner.privileged.write() = usernames.iter().map(|u| u.to_lowercase()).collect();
        }
        FromServer::UserStatus {
            username,
            privileged: true,
            ..
        } => {
            inner.privileged.write().insert(username.to_lowercase());
        }
        _ => {}
    }
    inner
        .metrics
        .peer_connections
        .store(inner.peers.links.len() as u64, Ordering::Relaxed);
}

/// The length of a ban the server announces: "You have been banned for 30
/// minutes."
fn ban_minutes(message: &str) -> Option<u64> {
    let rest = message.split("banned for ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let minutes = digits.parse().ok()?;
    rest[digits.len()..]
        .trim_start()
        .starts_with("minute")
        .then_some(minutes)
}

#[cfg(test)]
mod ban_tests {
    use super::ban_minutes;

    #[test]
    fn reads_the_ban_length() {
        let m = "System Message: You have been banned for 30 minutes. This is usually the result of doing too many operations at once.";
        assert_eq!(ban_minutes(m), Some(30));
        assert_eq!(ban_minutes("banned for life"), None);
        assert_eq!(ban_minutes("hello"), None);
    }
}
