//! Server-side sessions and login flows, keyed by opaque ids.
//!
//! Cookies carry an id and nothing else — no JWT, nothing forgeable, nothing
//! claim-bearing on the client. Sessions are rows in Postgres, keyed by the
//! id's SHA-256, so a deploy signs no one out and the table alone signs no one
//! in. A login flow lasts minutes and is used once, so it stays in memory: a
//! restart mid-login costs one retry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct Session {
    /// Hydra's subject. Not shown anywhere; it is what makes a session an
    /// identity rather than a bare permit.
    pub sub: String,
}

/// A login round-trip in progress: the PKCE verifier and the CSRF state, held
/// server-side so neither travels through the browser.
#[derive(Clone, Debug)]
pub struct Flow {
    pub verifier: String,
    pub csrf: String,
    pub expires: Instant,
}

#[derive(Clone)]
pub struct Sessions {
    db: PgPool,
    flows: Arc<RwLock<HashMap<String, Flow>>>,
}

fn hash(id: &str) -> Vec<u8> {
    Sha256::digest(id.as_bytes()).to_vec()
}

impl Sessions {
    pub fn new(db: PgPool) -> Self {
        Self {
            db,
            flows: Arc::default(),
        }
    }

    /// A URL-safe opaque id with 128 bits behind it.
    pub fn token() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    pub async fn create(&self, sub: &str, ttl: Duration) -> sqlx::Result<String> {
        let id = Self::token();
        // Expired rows go on each sign-in; there are too few to need a sweeper.
        sqlx::query("DELETE FROM ui_sessions WHERE expires_at < now()")
            .execute(&self.db)
            .await?;
        sqlx::query(
            "INSERT INTO ui_sessions (id_hash, sub, expires_at) \
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(hash(&id))
        .bind(sub)
        .bind(ttl.as_secs_f64())
        .execute(&self.db)
        .await?;
        Ok(id)
    }

    /// `None` for an unknown or expired id, and when the database cannot be
    /// asked: a sign-in page is the safe answer to not knowing.
    pub async fn get(&self, id: &str) -> Option<Session> {
        sqlx::query_scalar::<_, String>(
            "SELECT sub FROM ui_sessions WHERE id_hash = $1 AND expires_at > now()",
        )
        .bind(hash(id))
        .fetch_optional(&self.db)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "session lookup failed"))
        .ok()
        .flatten()
        .map(|sub| Session { sub })
    }

    pub async fn delete(&self, id: &str) {
        if let Err(e) = sqlx::query("DELETE FROM ui_sessions WHERE id_hash = $1")
            .bind(hash(id))
            .execute(&self.db)
            .await
        {
            tracing::warn!(error = %e, "session delete failed");
        }
    }

    pub async fn begin_flow(&self, verifier: &str, csrf: &str, ttl: Duration) -> String {
        let id = Self::token();
        let flow = Flow {
            verifier: verifier.to_string(),
            csrf: csrf.to_string(),
            expires: Instant::now() + ttl,
        };
        self.flows.write().await.insert(id.clone(), flow);
        id
    }

    /// One-shot: taking a flow consumes it, so a callback cannot be replayed.
    pub async fn take_flow(&self, id: &str) -> Option<Flow> {
        let flow = self.flows.write().await.remove(id)?;
        (Instant::now() < flow.expires).then_some(flow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Sessions {
        Sessions::new(PgPool::connect_lazy("postgres://unused").unwrap())
    }

    #[tokio::test]
    async fn take_flow_returns_once_then_none() {
        let sessions = sessions();
        let id = sessions
            .begin_flow("verifier", "csrf", Duration::from_secs(60))
            .await;
        assert!(sessions.take_flow(&id).await.is_some());
        assert!(sessions.take_flow(&id).await.is_none());
    }

    #[test]
    fn two_tokens_differ() {
        assert_ne!(Sessions::token(), Sessions::token());
    }

    #[test]
    fn the_stored_key_is_not_the_cookie() {
        let id = Sessions::token();
        assert_ne!(hash(&id), id.as_bytes());
        assert_eq!(hash(&id), hash(&id));
    }
}
