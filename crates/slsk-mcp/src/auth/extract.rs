//! The gate, and the accessor behind it.
//!
//! [`require_session`] is a layer over the whole protected router rather than a
//! decorator on each route. aria2's RPC is unauthenticated on the pod's
//! loopback by design, so any path reaching it without passing through here is
//! full control of the queue and write access to the media tree — which makes
//! "somebody adds a route and forgets" the failure worth designing out. Wrapped
//! at the router, a new route is protected by default and an unprotected one
//! has to be mounted somewhere else on purpose.
//!
//! [`CurrentSession`] then only reads what the layer already put in the request,
//! so it cannot disagree with the check.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::auth::cookie::{SESSION_COOKIE, read_cookie};
use crate::auth::session::Session;
use crate::ui::UiState as AppState;

/// Whether an unauthenticated request should be redirected or refused.
///
/// A page load wants the login round-trip. Anything the page itself issues
/// wants a status: Datastar follows redirects like any fetch, so a 302 on a
/// fragment request swaps the login page into the middle of the app, and an SSE
/// stream cannot be redirected at all. Datastar marks its own requests, which
/// is what makes this decidable rather than guessed.
pub fn is_page_load(headers: &axum::http::HeaderMap) -> bool {
    !headers.contains_key("datastar-request")
        && !headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/event-stream"))
}

pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let session = request
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| read_cookie(h, SESSION_COOKIE))
        .map(str::to_string);

    let session = match session {
        Some(id) => state.sessions.get(&id).await,
        // Local development without an identity provider: everyone is "dev".
        None if state.config.oidc.is_none() => Some(Session {
            sub: "dev".into(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        }),
        None => None,
    };

    match session {
        Some(session) => {
            tracing::debug!(sub = %session.sub, path = %request.uri().path(), "request");
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        None if is_page_load(request.headers()) => Redirect::to("/auth/login").into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The signed-in session, put here by [`require_session`].
#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::*;

    #[test]
    fn a_datastar_request_is_not_a_page_load() {
        let mut headers = HeaderMap::new();
        headers.insert("datastar-request", "true".parse().unwrap());
        assert!(!is_page_load(&headers));
    }

    #[test]
    fn an_event_stream_accept_is_not_a_page_load() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            "text/event-stream".parse().unwrap(),
        );
        assert!(!is_page_load(&headers));
    }

    #[test]
    fn an_ordinary_browser_accept_is_a_page_load() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            "text/html,application/xhtml+xml".parse().unwrap(),
        );
        assert!(is_page_load(&headers));
    }
}
