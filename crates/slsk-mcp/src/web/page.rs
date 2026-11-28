//! The pages, one module per tab, named as the nav names them. Every page has
//! the same shape ([`Page`]): the tab it belongs to, the stream that keeps it
//! current, what it reads from the request, and its body. [`serve`] draws any
//! of them: a browser asking for the address gets the layout around the body;
//! the page itself asking (a link or form that loads in place) gets the body
//! alone, morphed into `main`, with the address it now shows.

use std::future::Future;

use askama::Template;
use askama_datastar::Html;
use axum::extract::{FromRequestParts, OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Redirect, Response};

use super::layout::{Layout, Main, Nav, Status, Title};
pub(super) use askama_datastar::View;

use koan_kit_web as kit;

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

/// Page `P`'s body, and the live topics' counts as they stood before it was
/// drawn: the page's stream draws what changed after them, and nothing the
/// body already drew.
async fn draw<P: Page>(s: &UiState, args: P::Args) -> anyhow::Result<(Body, crate::live::Seen)> {
    let seen = crate::live::Seen::of(*crate::live::subscribe().borrow());
    Ok((P::body(s, args).await?, seen))
}

/// The signals a page starts with: its own, the address it shows, and the
/// counts its stream carries on from.
fn signals(body: &mut Body, url: &str, seen: crate::live::Seen) -> serde_json::Value {
    let mut signals = serde_json::Value::Object(std::mem::take(&mut body.signals));
    if let Some(all) = signals.as_object_mut() {
        all.insert("url".into(), url.into());
        all.insert("seen".into(), serde_json::json!(seen));
    }
    signals
}

/// Page `P` shown in place, as a link or form that loads in place is
/// answered: its body morphed into `main`, the title, any flash cleared, and
/// its signals. An action that takes the page somewhere answers with these
/// too.
pub(super) async fn shown<P: Page>(
    s: &UiState,
    args: P::Args,
    url: &str,
) -> anyhow::Result<Vec<Event>> {
    let (mut body, seen) = draw::<P>(s, args).await?;
    if let Some(to) = body.redirect {
        return Ok(vec![askama_datastar::go(&to).into()]);
    }
    let signals = signals(&mut body, url, seen);
    Ok(vec![
        super::patch(&Html::from_iter([
            Main { body: body.html }.draw(),
            Title::of(P::TAB, body.heading.as_deref()).draw(),
            kit::Flash::region().html(),
        ])),
        super::signals(signals),
    ])
}

/// Draw page `P`: whole for a browser, or in place for the page itself.
pub(super) async fn serve<P: Page>(
    State(s): State<UiState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    args: P::Args,
) -> Response {
    let url = &askama_datastar::address(&uri);
    if askama_datastar::is_datastar(&headers) {
        return match shown::<P>(&s, args, url).await {
            Ok(events) => askama_datastar::events(events),
            Err(e) => super::failed(&e),
        };
    }
    let (mut body, seen) = match draw::<P>(&s, args).await {
        Ok(drawn) => drawn,
        Err(e) => return crate::error::AppError::from(e).into_response(),
    };
    if let Some(to) = body.redirect {
        return Redirect::to(&to).into_response();
    }
    // Every page has the signals its stream reads, so every stream reads the same names.
    let mut all = serde_json::json!({
        "files": "",
        "jobs": super::PAGE,
        "history": super::PAGE,
        "librows": 0,
    });
    if let (Some(all), serde_json::Value::Object(own)) =
        (all.as_object_mut(), signals(&mut body, url, seen))
    {
        all.extend(own);
    }
    let layout = Layout {
        title: Title::of(P::TAB, body.heading.as_deref()).draw(),
        status: Status::of(&s.app).draw(),
        nav: Nav::of(&s.app, P::TAB).await.draw(),
        stream: P::STREAM,
        signals: all.to_string(),
        main: Main { body: body.html }.draw(),
        now: chrono::Utc::now().timestamp_millis(),
    };
    match Html::render(&layout) {
        Ok(html) => (body.status, html).into_response(),
        Err(e) => crate::error::AppError::from(anyhow::Error::from(e)).into_response(),
    }
}
