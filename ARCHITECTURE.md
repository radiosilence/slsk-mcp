# Architecture

slsk-mcp is one binary: a Soulseek engine, the jobs that carry an album from a
peer into the library, and three ways to drive them (a web UI, GraphQL and MCP).
This document describes how the service is put together, and the web UI in
detail, since radiosilence's other server-rendered apps are built the same way.

## The service

`main.rs` builds one `App` (configuration, database, session, jobs, social,
library, activity) and serves it on three listeners: the web UI, an internal
listener for GraphQL and MCP, and metrics. Everything the UI, GraphQL and MCP do
goes through the same calls on `App`; none of them owns logic the others lack.

- **Configuration** is read once at start from settings named as the
  environment names them (`Config::from_env`, or `Config::from_vars` with a
  lookup of one's own, as the tests do).
- **Auth** is a session layer over the whole UI router
  (`auth::extract::require_session`), so a route added later is protected by
  construction. Sign-in is OIDC against the estate's issuer (`auth::oidc`).
- **Background work** runs in tasks the services own: `jobs` downloads and
  imports, `social` keeps rooms and the wishlist, `library` indexes the
  library, and the web UI's library runs (`web::page::library::run`) apply
  library actions one at a time, saving each album as it finishes so a
  restart resumes them.
- **Change notification** is `live`: a counter per kind of thing a page shows
  (`Topic`), bumped by database writes to its tables and by the services when
  in-memory state changes. The engine has its own signal for transfers and the
  connection. Nothing polls.
- **MCP** is two tools, the GraphQL schema and a query, so an assistant drives
  the service through the same API as anything else.

## The web UI

Server-rendered HTML, patched over server-sent events by
[Datastar](https://data-star.dev). Markup comes from askama templates and the
components of [koan-kit-web](https://github.com/radiosilence/koan/tree/main/koan-kit-web);
the glue between them is [askama_datastar](https://github.com/radiosilence/askama-datastar).

```
crates/slsk-mcp/src/web.rs            the route table, shared state, assets, helpers
crates/slsk-mcp/src/web/layout.rs     the document, title, status line and nav
crates/slsk-mcp/src/web/page.rs       the Page shape, Body, and serve
crates/slsk-mcp/src/web/page/<tab>.rs one module per tab in the nav
crates/slsk-mcp/src/web/stream.rs     the Live shape and serve
crates/slsk-mcp/src/web/stream/<tab>.rs one module per page that has live parts
crates/slsk-mcp/src/web/peer.rs       the Ban button every page naming a peer shows
crates/slsk-mcp/templates/layout.html and layout/   the document and its regions
crates/slsk-mcp/templates/<tab>/page.html           each tab's body
crates/slsk-mcp/templates/<tab>/<fragment>.html     the regions it sends again
```

A tab's module, its stream and its templates carry the name the nav shows:
Downloads is `page/downloads.rs`, `stream/downloads.rs` and
`templates/downloads/`. A page under a tab, such as a job's, is a submodule of
the tab (`page/downloads/job.rs`).

### The route table

`web::routes` declares every route in one place, in three groups:

- **Pages**: `GET /<tab>` to `page::serve::<Tab>`.
- **Streams**: `GET /stream/<tab>` to `stream::serve::<Live>`. Every page has
  one, since every page shows the status line and nav; a page with nothing of
  its own live uses `stream::Quiet`.
- **Actions**: everything a page posts to or reads in place, each a handler in
  its page's module.

All of it sits behind `require_datastar_on_post` (a POST Datastar did not send
is refused, which stops cross-site forgery) and the session layer.

### Pages

Every page implements `page::Page`:

```rust
impl Page for Wishlist {
    const TAB: Tab = Tab::Wishlist;               // the nav entry it is shown under
    const STREAM: &'static str = "/stream/wishlist"; // the stream it holds open
    type Args = ();                               // what it reads from the request
    async fn body(s: &UiState, (): ()) -> anyhow::Result<Body> {
        Ok(Body::of(&WishlistPage { list: Wishes::of(&s.app).await.draw() }))
    }
}
```

`Args` is any axum extractor: `askama_datastar::View<Q>` reads the address's
query leniently (an address that does not parse is the default view, not an
error), and
`Path<Uuid>` names a job. `Body` carries the rendered body and what the layout
needs: a heading for the title, a status, the page's own signals, or a
redirect when the address now shows another page.

`page::serve::<P>` draws any page two ways. A browser asking for the address
gets the layout around the body. The page itself asking, through a link or
form that loads in place (`kit::Link::in_place`, `kit::Form::get`), gets the
body morphed into `main`, the title, and the address as `$url`
(`askama_datastar::address`, the request's address less Datastar's own
`datastar` parameter, spelled as the person wrote it). The layout's
`askama_datastar::history("url")` keeps the address bar on `$url`. An action
that takes the page elsewhere answers the same way, through `page::shown`, or
with `askama_datastar::go` when it must load another page whole. So a reload,
a shared link, back and forward all draw exactly what the address names.

### Streams

Every live stream implements `stream::Live`:

```rust
impl Live for Wishlist {
    const TAB: Tab = Tab::Wishlist;
    async fn open(s: &UiState, shown: Shown) -> Self { … }       // from the page's signals
    async fn draw(&mut self, s: &UiState, c: &Changed) -> Vec<Event> { … }
    fn wakes(&self) -> Option<BoxStream<'static, ()>> { … }  // optional: changes of its own
}
```

`stream::serve::<L>` runs it: it opens the stream from the page's signals and
turns what changes the page (the `live` topics, transfers, the connection,
the minute turning, and the stream's own `wakes`, such as a running search's
answers) into a stream of `Changed`, which `askama_datastar::paced` spaces at
most one every 250 ms and ends after a minute. Each `Changed` draws the status
line and nav, then the page's regions; it says what moved, so each stream
draws only the regions that could have changed, and `askama_datastar::Last`
(through `stream::last`, which compares markup without the busy signals the
kit names afresh on each render) sends a region only when it differs from
what was last sent.

A page opens its stream through `askama_datastar::reopen_on`, which opens it
again when it ends or goes quiet, and afresh when `$url` changes. A stream
therefore never tracks navigation: it draws the page its address names, and is
replaced when the address moves. Ending after a minute bounds how long a page
can sit on a connection that died without either end noticing. Every signal
`reopen_on` names (`url`, `jobs`, `history`, `files`, `librows`) is declared on
every page: an undeclared one stops the stream reopening without a console
message.

Because streams open so often, two rules keep an opening cheap.

**A stream draws only what changed after the page did.** A page records the
`live` topics' counts as they stood before its body was drawn, with the boot
they were counted in (the `seen` signal, `live::Seen`), and its stream's
first pass draws only the topics that moved since. The counts restart with
the process, so a `seen` from another boot matches nothing and the page is
drawn whole. After a pass the stream sets `seen` to the counts it has drawn up
to, so the stream that replaces it carries on from there. A pass that could
not read what it draws (`Live::failed`: the database did not answer) ends the
stream with `seen` where it was, so no later pass hands over counts it never
drew, and the stream the page opens next draws it again. Nothing the body drew,
or the last stream sent, is sent again.

**Work a page asks for is owned by the server, never by a stream.** A
question to a peer or the server (a share list, a profile, whether a user is
online) is asked through `peer::Peers`, an `asked::Asked` per kind of
question: asked once however many pages want it at the same moment, and its
answer remembered for a while, a refusal included, so neither reopening nor
moving within a page asks again. The page body asks; when the answer lands,
`Asked` announces the `Peers` topic and the streams showing it draw it. A
stream never starts work: it follows a running search (`graphql::listening`)
but never sends one, and a search's own run asks who is online behind what it
found. Refresh is the one way to ask again: it forgets the answer and loads
the address afresh.

### Actions

An action changes something and answers with as little as it can:

- When the page's stream draws what changed, the action answers with a flash
  or nothing, and closes the page's question (`kit::Confirm::close`) if one
  was asked. The change arriving is the confirmation. Downloads, search tabs,
  the wishlist, chat and uploads work this way.
- When nothing streams the region (bans, settings), the action answers with
  the region it changed.
- When the action moves the page (opening a conversation, leaving a room), it
  answers with the page it leads to (`page::shown`).
- A region the server filled that the person closes (a preview, an edit form)
  is closed by an action answering `askama_datastar::clear("#region")`, which
  empties it in place. The region's empty markup is declared once, in the
  template that places it (`library/closed.html`).

Form bodies whose fields repeat are read with `web::fields`; a button's row is
named in its URL's query, since a button posts the page's signals rather than
a form.

### Templates and markup

Every view is a struct deriving `Template` with a file under `templates/`;
there are no inline template sources. A view embeds another as
`askama_datastar::Html` (rendered with `Draw::draw`), which askama writes as
markup, and escapes every other value. Nothing builds markup with `format!`,
nothing calls `Html::trusted`, and no template uses `|safe`. A wrapper is a
template, or a kit component with a slot (`{% filter kit::wrap(…) %}`).

Datastar expressions are constants. A value that differs per row goes in a
`data-*` attribute, escaped, and the expression reads `el.dataset`. Signals are
named by the program, never built from data. Datastar sends a request every signal
except those whose names start with `_`, so a signal the server reads (the
address `url`, the rows shown, `libseen`) has no underscore, and one only the
page reads (a form's text, the open art form) has one.

### Patch targets

Datastar morphs a region by the id of its top-level element, so ids exist only
where the server sends a region again, declared once in its template:

| Where | Ids |
|---|---|
| Layout | `title`, `status`, `nav`, `main` |
| Downloads | `jobs-live`, `jobs-settled`, each card's `job-<id>`, a card disclosure's own id |
| Job | `job` |
| Search | `search-tabs`, `results`, each result's `r-<id>` and its peer status `peer-<id>` |
| Uploads | `uploads-peers`, `uploads-history` |
| Activity | `activity-feed`, `activity-trends` |
| Wishlist, Bans, Settings | `wishlist`, `bans`, `settings` |
| Library | `library`, `lib-rows`, `lib-more`, `lib-run`, `lib-run-head`, `lib-run-rows`, `lib-preview`, each open album's slot and its `-artinfo`, `-retag`, `-edit`, `-preview` |
| Browse, Profile | `browse`, `profile` |
| Chat | `chat`, `lines`, `rooms` |

A region sent whole is preferred to many small ones. A repeated control that
changes everywhere at once (a peer's Ban button) is found by a `data-` key
instead of an id. Forms carry an id only where something outside them names
them: `kit::Button::sends`, or an input's `form=`.

### The kit

Every control, status, meter, notice, flash, empty state, disclosure and
question is a koan-kit-web component; templates add layout utilities only.
Buttons that send requests are busy for exactly as long as their own request
runs, grouped by `busy_key` where several act on one thing. Disclosures that
read their content when opened use `kit::Disclosure::loads` and are answered
with `kit::Disclosure::fill`. What the kit cannot yet express is tracked on
radiosilence/koan#2365.

### The guard

`web::escaping` builds the real service against the in-process test server
(`web::fixture`), fills every field a person or peer controls with
markup-shaped text, and requests every page (whole and in place), every
stream and every disclosure region through the router with
`askama_datastar::escaping::get`. `escaping::Guard`, told the hostile texts,
the templates directory and the element names the kit writes, finds what is
wrong with each answer: markup of the templates' own escaped (a fragment
spliced in as text, which shows a page of raw HTML), the text escaped twice,
or the text left as markup. Where a page shows the text, the test checks it
is there as `escaping::escaped` writes it. The seeding and the addresses are
slsk's own; a new page, stream or field a person controls belongs in it.
