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
    /// Where beets moves imported albums. Also shared by default.
    pub library_dir: PathBuf,
    /// Shared with the network. Defaults to the library.
    pub share_dirs: Vec<PathBuf>,
    /// Downloads land here, one directory per job, until imported.
    pub staging_dir: PathBuf,
    /// The beets database, its generated config, and spectrograms.
    pub state_dir: PathBuf,
    /// The Soulseek peer port. Must be reachable from the internet for other
    /// peers to connect directly; firewalled peers still work through the
    /// server, but two firewalled peers cannot reach each other at all.
    pub listen_port: u16,
    pub upload_slots: usize,
    /// MCP and GraphQL. Trusts credential headers, so it must only be
    /// reachable from the gateway.
    pub internal_addr: String,
    /// The web UI, behind OIDC.
    pub ui_addr: String,
    pub public_url: String,
    /// `None` only when `UI_INSECURE_NO_AUTH=1`, for local development.
    pub oidc: Option<Oidc>,
    pub beet_bin: String,
    /// Merged over the built-in beets config — paths, plugins, anything.
    pub beets_config: Option<PathBuf>,
    pub discogs_token: Option<String>,
    /// Hold albums whose analysis says lossy source for review instead of
    /// importing them.
    pub hold_transcodes: bool,
    pub llm_model: String,
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

impl Config {
    pub fn from_env() -> Result<Self> {
        let seal_key = base64::engine::general_purpose::STANDARD
            .decode(var("SEAL_KEY")?.trim())
            .context("SEAL_KEY must be base64")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("SEAL_KEY must decode to 32 bytes"))?;

        let oidc = match (opt("OIDC_ISSUER"), opt("OIDC_CLIENT_ID"), opt("OIDC_CLIENT_SECRET")) {
            (Some(issuer), Some(client_id), Some(client_secret)) => Some(Oidc {
                issuer: issuer.trim_end_matches('/').to_string(),
                client_id,
                client_secret,
            }),
            (None, None, None) if opt("UI_INSECURE_NO_AUTH").as_deref() == Some("1") => None,
            (None, None, None) => {
                bail!("OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET are required (or UI_INSECURE_NO_AUTH=1 for local development)")
            }
            _ => bail!("OIDC_ISSUER, OIDC_CLIENT_ID and OIDC_CLIENT_SECRET must be set together"),
        };

        let library_dir = PathBuf::from(or("LIBRARY_DIR", "/music"));
        let share_dirs = match opt("SHARE_DIRS") {
            Some(dirs) => dirs.split(',').map(|d| PathBuf::from(d.trim())).collect(),
            None => vec![library_dir.clone()],
        };

        Ok(Self {
            database_url: var("DATABASE_URL")?,
            seal_key,
            library_dir,
            share_dirs,
            staging_dir: PathBuf::from(or("STAGING_DIR", "/data/staging")),
            state_dir: PathBuf::from(or("STATE_DIR", "/data")),
            listen_port: or("LISTEN_PORT", "2234").parse().context("LISTEN_PORT")?,
            upload_slots: or("UPLOAD_SLOTS", "3").parse().context("UPLOAD_SLOTS")?,
            internal_addr: or("INTERNAL_ADDR", "0.0.0.0:8081"),
            ui_addr: or("UI_ADDR", "0.0.0.0:8080"),
            public_url: or("PUBLIC_URL", "http://127.0.0.1:8080").trim_end_matches('/').to_string(),
            oidc,
            beet_bin: or("BEET_BIN", "beet"),
            beets_config: opt("BEETS_CONFIG").map(PathBuf::from),
            discogs_token: opt("DISCOGS_TOKEN"),
            hold_transcodes: or("HOLD_TRANSCODES", "1") != "0",
            llm_model: or("LLM_MODEL", "claude-sonnet-5"),
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }
}
