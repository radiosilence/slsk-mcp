//! Everything read from the environment, once, at boot.
//!
//! A missing or malformed value fails startup rather than the request that
//! first needs it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine as _;

pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
}

pub struct Config {
    pub database_url: String,
    /// Seals credentials at rest. 32 bytes, base64.
    pub seal_key: [u8; 32],
    /// An account to log in with at boot, so the client shares from the
    /// moment it starts. Credentials from the gateway or the UI replace it.
    pub account: Option<(String, String)>,
    pub server: String,
    /// Where beets moves imported albums. Also shared by default.
    pub library_dir: PathBuf,
    pub share_dirs: Vec<PathBuf>,
    /// Downloads in progress, one directory per job.
    pub staging_dir: PathBuf,
    /// Finished downloads waiting for import — and, for anything the tagger
    /// could not place, waiting for a person. Named for the album, so it can
    /// be browsed. On the same filesystem as the library, an import is a
    /// rename rather than a copy.
    pub complete_dir: PathBuf,
    /// The beets database and config, and the share probe cache.
    pub state_dir: PathBuf,
    /// The Soulseek peer port. Must be reachable from the internet for other
    /// peers to connect directly.
    pub listen_port: u16,
    pub upload_slots: usize,
    pub upload_limit: u64,
    pub download_limit: u64,
    /// MCP, GraphQL and metrics. Trusts credential headers, so it must only
    /// be reachable from the gateway and the metrics scraper.
    pub internal_addr: String,
    /// The web UI, behind OIDC.
    pub ui_addr: String,
    pub public_url: String,
    /// `None` only when `UI_INSECURE_NO_AUTH=1`, for local development.
    pub oidc: Option<Oidc>,
    pub beet_bin: String,
    /// Merged over the built-in beets config.
    pub beets_config: Option<PathBuf>,
    pub discogs_token: Option<String>,
    pub description: String,
}

fn var(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}

fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn or(key: &str, default: &str) -> String {
    opt(key).unwrap_or_else(|| default.to_string())
}

fn num<T: std::str::FromStr>(key: &str, default: T) -> Result<T> {
    match opt(key) {
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("{key} must be a number")),
        None => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let seal_key = base64::engine::general_purpose::STANDARD
            .decode(var("SEAL_KEY")?.trim())
            .context("SEAL_KEY must be base64")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("SEAL_KEY must decode to 32 bytes"))?;

        let oidc = match (
            opt("OIDC_ISSUER"),
            opt("OIDC_CLIENT_ID"),
            opt("OIDC_CLIENT_SECRET"),
        ) {
            (Some(issuer), Some(client_id), Some(client_secret)) => Some(Oidc {
                issuer: issuer.trim_end_matches('/').to_string(),
                client_id,
                client_secret,
            }),
            (None, None, None) if opt("UI_INSECURE_NO_AUTH").as_deref() == Some("1") => None,
            (None, None, None) => bail!(
                "OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET are required (or UI_INSECURE_NO_AUTH=1 for local development)"
            ),
            _ => bail!("OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET must be set together"),
        };

        let account = match (opt("SLSK_USERNAME"), opt("SLSK_PASSWORD")) {
            (Some(u), Some(p)) => Some((u, p)),
            (None, None) => None,
            _ => bail!("SLSK_USERNAME and SLSK_PASSWORD must be set together"),
        };

        let library_dir = PathBuf::from(or("LIBRARY_DIR", "/music"));
        let share_dirs = match opt("SHARE_DIRS") {
            Some(dirs) => dirs.split(',').map(|d| PathBuf::from(d.trim())).collect(),
            None => vec![library_dir.clone()],
        };

        let ui_addr = or("UI_ADDR", "0.0.0.0:8080");
        if oidc.is_none() && !ui_addr.starts_with("127.0.0.1:") && !ui_addr.starts_with("[::1]:") {
            bail!(
                "UI_INSECURE_NO_AUTH serves the UI with no sign-in; UI_ADDR must then be loopback, not {ui_addr}"
            );
        }
        Ok(Self {
            database_url: var("DATABASE_URL")?,
            seal_key,
            account,
            server: or("SLSK_SERVER", "server.slsknet.org:2242"),
            library_dir,
            share_dirs,
            staging_dir: PathBuf::from(or("STAGING_DIR", "/data/incomplete")),
            complete_dir: PathBuf::from(or("COMPLETE_DIR", "/data/complete")),
            state_dir: PathBuf::from(or("STATE_DIR", "/data")),
            listen_port: num("LISTEN_PORT", 2234)?,
            upload_slots: num("UPLOAD_SLOTS", 5)?,
            upload_limit: num("UPLOAD_LIMIT", 0)?,
            download_limit: num("DOWNLOAD_LIMIT", 0)?,
            internal_addr: or("INTERNAL_ADDR", "0.0.0.0:8081"),
            ui_addr,
            public_url: or("PUBLIC_URL", "http://127.0.0.1:8080")
                .trim_end_matches('/')
                .to_string(),
            oidc,
            beet_bin: or("BEET_BIN", "beet"),
            beets_config: opt("BEETS_CONFIG").map(PathBuf::from),
            discogs_token: opt("DISCOGS_TOKEN"),
            description: or("DESCRIPTION", ""),
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }
}
