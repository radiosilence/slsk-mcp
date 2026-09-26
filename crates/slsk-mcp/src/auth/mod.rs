//! An OAuth client of the estate's Hydra, and nothing more.
//!
//! Hydra is already public at the MCP gateway's auth hostname with a GitHub
//! allowlist in front of it, so who may sign in is decided there. Anyone
//! holding a token from that issuer has already passed it, which is why there
//! is no second allowlist here — a copy would be a second thing to keep in
//! step, and the one that drifted would be this one.
//!
//! Sessions are kept in Postgres, as mcp-gateway keeps its own, so a deploy
//! does not sign anyone out.

pub mod cookie;
pub mod extract;
pub mod oidc;
pub mod routes;
pub mod session;
