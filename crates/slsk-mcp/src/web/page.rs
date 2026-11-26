//! The pages, one module per tab, named as the nav names them. Every page has
//! the same shape ([`Page`]): the tab it belongs to, the stream that keeps it
//! current, what it reads from the request, and its body. [`serve`] draws any
//! of them: a browser asking for the address gets the layout around the body;
//! the page itself asking (a link or form that loads in place) gets the body
//! alone, morphed into `main`, with the address it now shows.

use std::future::Future;

use askama::Template;
use askama_datastar::Html;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Redirect, Response};
use koan_kit_web as kit;

use super::layout::{Layout, Main, Nav, Status, Title};
use super::{Draw, UiState};

pub(super) mod activity;
pub(super) mod bans;
pub(super) mod browse;
pub(super) mod chat;
pub(super) mod downloads;
pub(super) mod library;
pub(super) mod profile;
pub(super) mod search;
pub(super) mod settings;
pub(super) mod triage;
pub(super) mod uploads;
pub(super) mod wishlist;

/// A tab in the nav.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tab {
    Search,
    Downloads,
    Uploads,
    Activity,
    Wishlist,
    Library,
    Browse,
    Chat,
    Profile,
    Bans,
    Settings,
    Triage,
}

impl Tab {
    /// In the order the nav lists them.
    pub(super) const ALL: &[Tab] = &[
        Tab::Search,
        Tab::Downloads,
        Tab::Uploads,
        Tab::Activity,
        Tab::Wishlist,
        Tab::Library,
        Tab::Browse,
        Tab::Chat,
        Tab::Profile,
        Tab::Bans,
        Tab::Settings,
        Tab::Triage,
    ];

    /// What the nav calls it.
    pub(super) fn label(self) -> &'static str {
        match self {
            Tab::Search => "search",
            Tab::Downloads => "downloads",
            Tab::Uploads => "uploads",
            Tab::Activity => "activity",
            Tab::Wishlist => "wishlist",
            Tab::Library => "library",
            Tab::Browse => "browse",
            Tab::Chat => "chat",
            Tab::Profile => "profile",
            Tab::Bans => "bans",
            Tab::Settings => "settings",
            Tab::Triage => "triage",
        }
    }

    pub(super) fn href(self) -> &'static str {
        match self {
            Tab::Downloads => "/",
            Tab::Search => "/search",
            Tab::Uploads => "/uploads",
            Tab::Activity => "/activity",
            Tab::Wishlist => "/wishlist",
            Tab::Library => "/library",
            Tab::Browse => "/browse",
            Tab::Chat => "/chat",
            Tab::Profile => "/profile",
            Tab::Bans => "/bans",
            Tab::Settings => "/settings",
            Tab::Triage => "/triage",
        }
    }
}

/// A page: the tab it is shown under, the stream that keeps it current, what
/// it reads from the request, and how its body is drawn.
pub(super) trait Page: Send + 'static {
    const TAB: Tab;
    /// The address of the live stream the page holds open.
    const STREAM: &'static str;
    type Args: FromRequestParts<UiState, Rejection: IntoResponse> + Send;
    fn body(s: &UiState, args: Self::Args) -> impl Future<Output = anyhow::Result<Body>> + Send;
}

/// A page's body as its module draws it, with what the layout needs to know of it.
pub(super) struct Body {
    html: Html,
    status: StatusCode,
    /// What to call the page when the tab's name does not say.
    heading: Option<String>,
    /// Signals of the page's own, beside those every page has.
    signals: serde_json::Map<String, serde_json::Value>,
    /// The address shows another page: where to go instead.
    redirect: Option<String>,
}

impl Body {
    pub(super) fn of(view: &impl Template) -> Self {
        Self {
            html: view.draw(),
            status: StatusCode::OK,
            heading: None,
            signals: Default::default(),
            redirect: None,
        }
    }

    /// The page that address now has.
    pub(super) fn redirect(to: String) -> Self {
        Self {
            html: Html::new(),
            status: StatusCode::OK,
            heading: None,
            signals: Default::default(),
            redirect: Some(to),
        }
    }

    pub(super) fn heading(mut self, heading: impl Into<Option<String>>) -> Self {
        self.heading = heading.into().filter(|h| !h.is_empty());
        self
    }

    pub(super) fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub(super) fn signal(mut self, name: &str, value: impl Into<serde_json::Value>) -> Self {
        self.signals.insert(name.to_string(), value.into());
        self
    }
}

/// The view an address's query describes, for a page. One that does not
/// parse (a repeated key, a value of the wrong type) is the default view
/// rather than an error: an address is something a person types or keeps.
pub(super) struct View<T>(pub(super) T);

impl<T: serde::de::DeserializeOwned + Default + Send, S: Sync> FromRequestParts<S> for View<T> {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(super::query(&parts.uri)))
    }
}

/// The events that show `body` in place, as an in-place load answers: its
/// body morphed into `main`, the title, any flash cleared, and its signals
/// with the address it now shows. An action that takes the page somewhere
/// answers with these too.
pub(super) fn shown<P: Page>(body: Body, url: &str) -> Vec<Event> {
    if let Some(to) = body.redirect {
        return vec![kit::Confirm::go(&to).into()];
    }
    let mut signals = serde_json::Value::Object(body.signals);
    if let Some(all) = signals.as_object_mut() {
        all.insert("url".into(), url.into());
    }
    vec![
        super::patch(&Html::from_iter([
            Main { body: body.html }.draw(),
            Title::of(P::TAB, body.heading.as_deref()).draw(),
            kit::Flash::region().html(),
        ])),
        super::signals(signals),
    ]
}

/// The page's address as the person sees it: the request's, less the
/// signals Datastar adds to a GET it sends.
fn address(uri: &Uri) -> String {
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .filter(|(k, _)| k != "datastar")
            .collect();
    let params: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    super::place_url(uri.path(), &params)
}

/// Draw page `P`: whole for a browser, or in place for the page itself.
pub(super) async fn serve<P: Page>(
    State(s): State<UiState>,
    headers: HeaderMap,
    uri: Uri,
    args: P::Args,
) -> Response {
    let in_place = askama_datastar::is_datastar(&headers);
    let body = match P::body(&s, args).await {
        Ok(b) => b,
        Err(e) if in_place => return super::failed(&e),
        Err(e) => return crate::error::AppError::from(e).into_response(),
    };
    let url = &address(&uri);
    if in_place {
        return askama_datastar::events(shown::<P>(body, url));
    }
    if let Some(to) = body.redirect {
        return Redirect::to(&to).into_response();
    }
    // Every page has the signals its stream reads, so every stream reads the same names.
    let mut signals = serde_json::json!({
        "url": url,
        "files": "",
        "jobs": super::PAGE,
        "history": super::PAGE,
    });
    if let Some(all) = signals.as_object_mut() {
        all.extend(body.signals);
    }
    let layout = Layout {
        title: Title::of(P::TAB, body.heading.as_deref()).draw(),
        status: Status::of(&s.app).draw(),
        nav: Nav::of(&s.app, P::TAB).await.draw(),
        stream: P::STREAM,
        signals: signals.to_string(),
        main: Main { body: body.html }.draw(),
        now: chrono::Utc::now().timestamp_millis(),
    };
    match Html::render(&layout) {
        Ok(html) => (body.status, html).into_response(),
        Err(e) => crate::error::AppError::from(anyhow::Error::from(e)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_address_leaves_out_what_datastar_adds() {
        let uri = "/search?tab=t1&datastar=%7B%22url%22%3A%22%2F%22%7D&shown=200"
            .parse()
            .unwrap();
        assert_eq!(super::address(&uri), "/search?tab=t1&shown=200");
        assert_eq!(super::address(&"/".parse().unwrap()), "/");
    }
}
