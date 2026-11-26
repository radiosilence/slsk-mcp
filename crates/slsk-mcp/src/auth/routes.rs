//! `/auth/login`, `/auth/callback`, `/auth/logout`.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::auth::cookie::{FLOW_COOKIE, SESSION_COOKIE, clear_cookie, read_cookie, set_cookie};
use crate::auth::oidc::{authorize_url, exchange_code, pkce};
use crate::auth::session::Sessions;
use crate::error::{AppError, AppResult};
use crate::web::UiState as AppState;

/// How long a signed-in session lasts. A week, because the alternative is
/// logging in from a sofa.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;
/// Long enough to complete a login round-trip and no longer.
pub const FLOW_TTL_SECS: i64 = 600;

pub fn router(sessions: Sessions) -> Router<AppState> {
    let router = Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .merge(signout().with_state(sessions));
    #[cfg(debug_assertions)]
    let router = router.route("/auth/dev-login", get(dev_login));
    router
}

/// Mints a session for a fixed "dev" subject and skips the OIDC round trip
/// entirely — nothing here reaches Hydra. Mounted alongside `login` and
/// `callback`, outside the auth layer, for the same reason they are: a login
/// route cannot itself require a session.
///
/// Gated on `#[cfg(debug_assertions)]` rather than a runtime flag or an env
/// var. A flag is a value that can end up set in production by accident — a
/// copied `.env`, a stray Helm value — and would need code here to check it
/// on every request. A compile-time gate cannot: `cargo build --release`,
/// what the Dockerfile runs, does not set `debug_assertions`, so this
/// function and the route that reaches it are simply not present in the
/// binary. There is nothing to bypass because there is nothing there.
#[cfg(debug_assertions)]
async fn dev_login(State(state): State<AppState>) -> AppResult<Response> {
    let session_id = state
        .sessions
        .create(
            "dev",
            std::time::Duration::from_secs(SESSION_TTL_SECS as u64),
        )
        .await
        .map_err(anyhow::Error::from)?;
    Ok(redirect(
        "/",
        &[set_cookie(SESSION_COOKIE, &session_id, SESSION_TTL_SECS)],
    ))
}

/// Build a redirect response, appending each cookie as its own `Set-Cookie`
/// header.
fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
    let headers = response.headers_mut();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            headers.append(header::SET_COOKIE, value);
        }
    }
    response
}

pub async fn login(State(state): State<AppState>) -> AppResult<Response> {
    let (verifier, challenge) = pkce();
    let csrf = Sessions::token();
    let flow_id = state.sessions.begin_flow(
        &verifier,
        &csrf,
        std::time::Duration::from_secs(FLOW_TTL_SECS as u64),
    );
    let url = authorize_url(
        state.config.oidc.as_ref().ok_or(AppError::Unauthorized)?,
        &state.config.redirect_uri(),
        &csrf,
        &challenge,
    );
    Ok(redirect(
        &url,
        &[set_cookie(FLOW_COOKIE, &flow_id, FLOW_TTL_SECS)],
    ))
}

#[derive(serde::Deserialize)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> AppResult<Response> {
    let flow_id = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| read_cookie(h, FLOW_COOKIE))
        .ok_or(AppError::Unauthorized)?;
    let flow = state
        .sessions
        .take_flow(flow_id)
        .ok_or(AppError::Unauthorized)?;
    if flow.csrf != q.state {
        return Err(AppError::BadRequest("state mismatch".to_string()));
    }

    let sub = exchange_code(
        &state.http,
        state.config.oidc.as_ref().ok_or(AppError::Unauthorized)?,
        &state.config.redirect_uri(),
        &q.code,
        &flow.verifier,
    )
    .await?;
    let session_id = state
        .sessions
        .create(
            &sub,
            std::time::Duration::from_secs(SESSION_TTL_SECS as u64),
        )
        .await
        .map_err(anyhow::Error::from)?;

    Ok(redirect(
        "/",
        &[
            set_cookie(SESSION_COOKIE, &session_id, SESSION_TTL_SECS),
            clear_cookie(FLOW_COOKIE),
        ],
    ))
}

/// Sign-out is a POST, since on a GET any page could sign a user out with an
/// image tag. It needs only the sessions, which is what lets its tests run
/// without the rest of the app.
fn signout() -> Router<Sessions> {
    Router::new().route("/auth/logout", post(logout))
}

/// Whether a state-changing request came from this site's own pages: Datastar
/// marks its requests with a header a cross-site form cannot set, and a plain
/// form is known by the browser's `Sec-Fetch-Site`, or else by an `Origin`
/// naming the host it was sent to. A request carrying none of them is refused.
fn from_this_site(headers: &HeaderMap) -> bool {
    if askama_datastar::is_datastar(headers) {
        return true;
    }
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(site) = text(header::HeaderName::from_static("sec-fetch-site")) {
        return site == "same-origin";
    }
    match (text(header::ORIGIN), text(header::HOST)) {
        (Some(origin), Some(host)) => origin
            .split_once("://")
            .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host)),
        _ => false,
    }
}

pub async fn logout(State(sessions): State<Sessions>, headers: HeaderMap) -> Response {
    if !from_this_site(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(id) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| read_cookie(h, SESSION_COOKIE))
    {
        sessions.delete(id).await;
    }
    // See Other, so the browser follows the POST with a GET of the page.
    let mut response = redirect("/", &[clear_cookie(SESSION_COOKIE)]);
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The sign-out routes served on a loopback port, with one session
    /// signed in, and a client that does not follow the redirect.
    async fn serve() -> (String, Sessions, String, reqwest::Client) {
        let sessions = Sessions::new(crate::db::Db::memory().await);
        let id = sessions
            .create("someone", Duration::from_secs(60))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = signout().with_state(sessions.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        (base, sessions, id, client)
    }

    #[tokio::test]
    async fn a_get_does_not_sign_out() {
        let (base, sessions, id, client) = serve().await;
        let response = client
            .get(format!("{base}/auth/logout"))
            .header(header::COOKIE, format!("{SESSION_COOKIE}={id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(sessions.get(&id).await.is_some());
    }

    #[tokio::test]
    async fn a_post_from_another_site_does_not_sign_out() {
        let (base, sessions, id, client) = serve().await;
        for (name, value) in [
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://evil.example"),
        ] {
            let response = client
                .post(format!("{base}/auth/logout"))
                .header(header::COOKIE, format!("{SESSION_COOKIE}={id}"))
                .header(name, value)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}");
        }
        let response = client
            .post(format!("{base}/auth/logout"))
            .header(header::COOKIE, format!("{SESSION_COOKIE}={id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "no headers");
        assert!(sessions.get(&id).await.is_some());
    }

    #[tokio::test]
    async fn a_post_from_this_site_signs_out() {
        for (name, value) in [
            ("datastar-request", "true"),
            ("sec-fetch-site", "same-origin"),
        ] {
            let (base, sessions, id, client) = serve().await;
            let response = client
                .post(format!("{base}/auth/logout"))
                .header(header::COOKIE, format!("{SESSION_COOKIE}={id}"))
                .header(name, value)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{name}");
            assert!(sessions.get(&id).await.is_none(), "{name}");
        }
        let (base, sessions, id, client) = serve().await;
        let origin = base.clone();
        let response = client
            .post(format!("{base}/auth/logout"))
            .header(header::COOKIE, format!("{SESSION_COOKIE}={id}"))
            .header(header::ORIGIN, origin)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "origin");
        assert!(sessions.get(&id).await.is_none(), "origin");
    }
}
