//! A peer's Ban button, on every page that names a peer: search results,
//! uploads, profiles, shares and job cards.

use askama_datastar::ElementPatchMode;
use axum::extract::{Query, State};
use axum::response::Response;
use koan_kit_web as kit;

use super::{UiState, failed, insert, place_url};

/// A peer an action names in its address, since a button posts the page's
/// signals rather than a form.
#[derive(serde::Deserialize)]
pub(super) struct PeerQuery {
    pub(super) username: String,
}

/// The key a peer's Ban button carries, so one press redraws every copy of
/// it on the page whatever characters the name holds.
fn key(username: &str) -> String {
    username.bytes().map(|b| format!("{b:02x}")).collect()
}

/// A Ban button for a peer, or Unban once banned. Banning asks first: it
/// refuses the peer everything and stops them being downloaded from.
pub(super) fn ban_button(username: &str, banned: bool) -> kit::Button<'static> {
    let (button, action) = if banned {
        (kit::Button::quiet("Unban"), "/peer/unban")
    } else {
        (
            kit::Button::quiet("Ban")
                .bad()
                .confirm("Ban this user?", "Ban")
                .detail("They get nothing more from you, and nothing is downloaded from them."),
            "/peer/ban",
        )
    };
    button
        .post(place_url(action, &[("username", username)]))
        .data("peer-ban", key(username))
}

pub(super) async fn ban(State(s): State<UiState>, Query(p): Query<PeerQuery>) -> Response {
    set(&s, &p.username, true).await
}

pub(super) async fn unban(State(s): State<UiState>, Query(p): Query<PeerQuery>) -> Response {
    set(&s, &p.username, false).await
}

/// Every copy of the peer's button on the page swapped for its other state.
/// They are found by the key they carry rather than an id, since one peer's
/// button can appear many times.
async fn set(s: &UiState, username: &str, banned: bool) -> Response {
    if let Err(e) = crate::graphql::set_ban(&s.app, username, banned).await {
        return failed(&anyhow::anyhow!("{}", e.message));
    }
    let said = if banned {
        format!("Banned {username}")
    } else {
        format!("Unbanned {username}")
    };
    askama_datastar::events([
        insert(
            &format!("[data-peer-ban=\"{}\"]", key(username)),
            ElementPatchMode::Replace,
            &ban_button(username, banned).html(),
        ),
        kit::Flash::ok(&said).patch().into(),
        kit::Confirm::close().into(),
    ])
}
