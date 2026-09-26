//! A Soulseek server small enough to run inside a test.
//!
//! It does what clients need from the server to find and reach each other —
//! login, addresses, indirect connection relay, search fan-out, private
//! messages — and nothing else: no rooms, no distributed network, no stats.
//! Peers still talk to each other directly, so a test exercises the real
//! peer protocol end to end.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use slsk_proto::frame::{CodeWidth, decode, encode};
use slsk_proto::server::mock;
use slsk_proto::wire::{Reader, Writer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

struct User {
    conn: u64,
    tx: mpsc::UnboundedSender<Bytes>,
    ip: Ipv4Addr,
    port: u32,
}

#[derive(Default)]
struct State {
    online: HashMap<String, User>,
    passwords: HashMap<String, String>,
    next: u64,
}

pub struct TestServer {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let task = tokio::spawn(async move {
            while let Ok((stream, peer)) = listener.accept().await {
                tokio::spawn(client(state.clone(), stream, peer));
            }
        });
        Self { addr, task }
    }

    /// `host:port`, as a client's server setting wants it.
    pub fn address(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn frame(code: u32, build: impl FnOnce(&mut Writer)) -> Bytes {
    let mut w = Writer::new();
    build(&mut w);
    encode(CodeWidth::U32, code, &w.finish())
}

async fn client(state: Arc<Mutex<State>>, stream: TcpStream, peer: SocketAddr) {
    let ip = match peer.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(_) => Ipv4Addr::LOCALHOST,
    };
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        while let Some(b) = rx.recv().await {
            if wr.write_all(&b).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let mut buf = BytesMut::new();
    let mut me: Option<(String, u64)> = None;
    loop {
        let f = loop {
            match decode(&mut buf, CodeWidth::U32, 1 << 20) {
                Ok(Some(f)) => break Some(f),
                Ok(None) => {}
                Err(_) => break None,
            }
            match rd.read_buf(&mut buf).await {
                Ok(0) | Err(_) => break None,
                Ok(_) => {}
            }
        };
        let Some(f) = f else { break };
        let mut r = Reader::new(f.body);
        let Ok(()) = handle(&state, &tx, ip, &mut me, f.code, &mut r) else {
            continue;
        };
    }
    if let Some((name, conn)) = me {
        let mut s = state.lock().unwrap();
        if s.online.get(&name).is_some_and(|u| u.conn == conn) {
            s.online.remove(&name);
        }
    }
}

fn handle(
    state: &Mutex<State>,
    tx: &mpsc::UnboundedSender<Bytes>,
    ip: Ipv4Addr,
    me: &mut Option<(String, u64)>,
    code: u32,
    r: &mut Reader,
) -> slsk_proto::wire::Result<()> {
    let mut s = state.lock().unwrap();
    match code {
        1 => {
            let username = r.string()?;
            let password = r.string()?;
            let known = s
                .passwords
                .entry(username.clone())
                .or_insert_with(|| password.clone())
                .clone();
            if known != password {
                let _ = tx.send(frame(1, |w| {
                    w.bool(false).str("INVALIDPASS");
                }));
                return Ok(());
            }
            s.next += 1;
            let conn = s.next;
            if let Some(old) = s.online.insert(
                username.clone(),
                User {
                    conn,
                    tx: tx.clone(),
                    ip,
                    port: 0,
                },
            ) {
                let _ = old.tx.send(frame(41, |_| {}));
            }
            let _ = tx.send(mock::login_ok("welcome", ip));
            *me = Some((username, conn));
        }
        2 => {
            let port = r.u32()?;
            if let Some((name, _)) = me {
                if let Some(u) = s.online.get_mut(name) {
                    u.port = port;
                }
            }
        }
        3 => {
            let username = r.string()?;
            let reply = match s.online.get(&username) {
                Some(u) => mock::peer_address(&username, u.ip, u.port),
                None => mock::peer_address(&username, Ipv4Addr::UNSPECIFIED, 0),
            };
            let _ = tx.send(reply);
        }
        18 => {
            let token = r.u32()?;
            let target = r.string()?;
            let kind =
                slsk_proto::ConnKind::parse(&r.string()?).unwrap_or(slsk_proto::ConnKind::Peer);
            let Some((name, _)) = me.as_ref() else {
                return Ok(());
            };
            let Some(from) = s.online.get(name) else {
                return Ok(());
            };
            let msg = mock::connect_to_peer(name, kind, from.ip, from.port, token);
            match s.online.get(&target) {
                Some(t) => {
                    let _ = t.tx.send(msg);
                }
                None => {
                    let _ = tx.send(frame(1001, |w| {
                        w.u32(token);
                    }));
                }
            }
        }
        22 => {
            let to = r.string()?;
            let message = r.string()?;
            let Some((from, _)) = me.as_ref() else {
                return Ok(());
            };
            if let Some(t) = s.online.get(&to) {
                let _ = t.tx.send(frame(22, |w| {
                    w.u32(1).u32(0).str(from).str(&message).bool(true);
                }));
            }
        }
        26 => {
            let token = r.u32()?;
            let query = r.string()?;
            let Some((from, _)) = me.as_ref() else {
                return Ok(());
            };
            for (name, u) in &s.online {
                if name != from {
                    let _ = u.tx.send(mock::file_search(from, token, &query));
                }
            }
        }
        42 => {
            let target = r.string()?;
            let token = r.u32()?;
            let query = r.string()?;
            let Some((from, _)) = me.as_ref() else {
                return Ok(());
            };
            if let Some(t) = s.online.get(&target) {
                let _ = t.tx.send(mock::file_search(from, token, &query));
            }
        }
        1001 => {
            let token = r.u32()?;
            let target = r.string()?;
            if let Some(t) = s.online.get(&target) {
                let _ = t.tx.send(frame(1001, |w| {
                    w.u32(token);
                }));
            }
        }
        _ => {}
    }
    Ok(())
}
