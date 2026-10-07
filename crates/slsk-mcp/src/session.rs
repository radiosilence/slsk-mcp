//! The one Soulseek session this service runs, and where its credentials
//! come from.
//!
//! Soulseek allows one login per account and the listen port is one port, so
//! there is one engine. It starts from whichever credentials arrive first —
//! the environment at boot, the account last used, or the first request that
//! carries them — and switches when a request brings different ones.

use std::sync::Arc;

use anyhow::{Context, Result};
use slsk_engine::{Engine, EngineConfig, Event, Status};
use tokio::sync::{Mutex, OnceCell};

use crate::config::Config;
use crate::crypto::Sealer;
use crate::db;

pub struct Session {
    engine: OnceCell<Engine>,
    start: Mutex<()>,
    cfg: Arc<Config>,
    db: crate::db::Db,
    sealer: Sealer,
}

impl Session {
    pub fn new(cfg: Arc<Config>, db: crate::db::Db) -> Arc<Self> {
        Arc::new(Self {
            sealer: Sealer::new(&cfg.seal_key),
            engine: OnceCell::new(),
            start: Mutex::new(()),
            cfg,
            db,
        })
    }

    pub fn engine(&self) -> Option<&Engine> {
        self.engine.get()
    }

    pub fn require(&self) -> Result<&Engine> {
        self.engine().context("no Soulseek account is configured yet: sign in from the web UI or send credentials through the gateway")
    }

    /// Log in at boot if there is anything to log in with.
    pub async fn boot(self: &Arc<Self>) -> Result<()> {
        if let Some((u, p)) = self.cfg.account.clone() {
            self.use_account(&u, &p).await?;
        } else if let Some((u, sealed)) = db::active_account(&self.db).await? {
            match self.sealer.open(&sealed) {
                Ok(p) => {
                    self.use_account(&u, &p).await?;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stored credentials could not be unsealed; waiting for new ones")
                }
            }
        }
        Ok(())
    }

    /// Make `username`/`password` the account in use, starting the engine if
    /// it is not running and persisting the credentials if they changed.
    pub async fn use_account(self: &Arc<Self>, username: &str, password: &str) -> Result<Engine> {
        let _guard = self.start.lock().await;
        if let Some(engine) = self.engine.get() {
            if engine.username() != username || !engine.password_matches(password) {
                tracing::info!(%username, "switching Soulseek account");
                engine.set_credentials(username, password);
                db::save_account(&self.db, username, &self.sealer.seal(password)).await?;
            }
            return Ok(engine.clone());
        }
        let mut ec = EngineConfig::new(username, password);
        ec.server = self.cfg.server.clone();
        ec.listen_port = self.cfg.listen_port;
        ec.share_dirs = self.cfg.share_dirs.clone();
        ec.state_dir = self.cfg.state_dir.clone();
        ec.upload_slots = self.cfg.upload_slots;
        ec.upload_limit = self.cfg.upload_limit;
        ec.download_limit = self.cfg.download_limit;
        ec.searches_per_hour = self.cfg.searches_per_hour;
        ec.description = self.cfg.description.clone();
        let engine = Engine::start(ec)
            .await
            .context("could not start the Soulseek engine")?;
        db::save_account(&self.db, username, &self.sealer.seal(password)).await?;
        self.engine.set(engine.clone()).ok();
        tokio::spawn(apply_bans_on_login(self.clone(), engine.clone()));
        Ok(engine)
    }
}

/// Bans live in the database per account; push them into the engine every
/// time a login lands, since the account may have changed.
async fn apply_bans_on_login(session: Arc<Session>, engine: Engine) {
    let mut events = engine.events();
    loop {
        match events.recv().await {
            Ok(Event::Status(Status::LoggedIn { .. })) => {
                match db::bans(&session.db, &engine.username()).await {
                    Ok(bans) => engine.set_banned(bans),
                    Err(e) => tracing::warn!(error = %e, "could not load bans"),
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => return,
        }
    }
}
