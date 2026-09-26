//! MCP: two tools, as the sibling servers have — the schema, and a query.
//!
//! The schema stays behind a tool call rather than in the always-loaded tool
//! descriptions, so a session that never mentions music pays almost nothing
//! for having this connected.

use std::sync::Arc;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};

use crate::App;
use crate::graphql::{self, SlskSchema};

type ToolResult = std::result::Result<CallToolResult, McpError>;

/// Set by the gateway after it has authenticated the user. Trusted without
/// question, which is why the port serving this is reachable from the
/// gateway alone.
pub const USERNAME_HEADER: &str = "x-slsk-username";
pub const PASSWORD_HEADER: &str = "x-slsk-password";

#[derive(Clone)]
pub struct SlskMcp {
    app: Arc<App>,
    schema: SlskSchema,
    #[allow(dead_code)] // read by the #[tool_handler] expansion
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GraphqlRequest {
    /// The GraphQL query or mutation.
    pub query: String,
    /// Variables, as a JSON object encoded in a string.
    pub variables: Option<String>,
}

/// Switch to the credentials a request carries, if it carries any.
pub async fn apply_credentials(app: &Arc<App>, headers: &http::HeaderMap) -> Result<(), String> {
    let get = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if let (Some(u), Some(p)) = (get(USERNAME_HEADER), get(PASSWORD_HEADER)) {
        app.session
            .use_account(u, p)
            .await
            .map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

impl SlskMcp {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            schema: graphql::schema(app.clone()),
            app,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl SlskMcp {
    #[tool(
        name = "slsk_schema",
        title = "Soulseek schema",
        description = "The Soulseek client's GraphQL schema. Call once before the first `slsk` query."
    )]
    async fn slsk_schema(&self) -> ToolResult {
        Ok(CallToolResult::success(vec![Content::text(graphql::sdl())]))
    }

    #[tool(
        name = "slsk",
        title = "Soulseek",
        description = "Run a GraphQL query or mutation against the user's Soulseek client, which shares their music library and files what it downloads into it. Use it to find and fetch albums (`grab`), search and browse other users, follow downloads and imports, resolve albums the tagger could not match, and manage uploads and bans. Get the schema from `slsk_schema` first. Variables are a JSON string."
    )]
    async fn slsk(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<GraphqlRequest>,
    ) -> ToolResult {
        if let Some(parts) = ctx.extensions.get::<http::request::Parts>()
            && let Err(e) = apply_credentials(&self.app, &parts.headers).await
        {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        let mut request = async_graphql::Request::new(req.query);
        if let Some(vars) = req.variables.filter(|v| !v.trim().is_empty()) {
            match serde_json::from_str::<serde_json::Value>(&vars) {
                Ok(v @ serde_json::Value::Object(_)) => {
                    request = request.variables(async_graphql::Variables::from_json(v))
                }
                Ok(_) => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "variables must be a JSON object",
                    )]));
                }
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "invalid variables JSON: {e}"
                    ))]));
                }
            }
        }
        let response = self.schema.execute(request).await;
        let json = serde_json::to_string_pretty(&response)
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for SlskMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(
                Implementation::new("slsk", env!("CARGO_PKG_VERSION"))
                    .with_title("Soulseek")
                    .with_website_url("https://github.com/radiosilence/slsk-mcp"),
            )
            .with_instructions(
                "The user's Soulseek client, as a GraphQL API. Read the schema once with `slsk_schema`, \
                 then query with `slsk`.\n\n\
                 To get an album into the library, call `grab(query: \"artist album\")`. It searches, picks the \
                 best lossless copy from a peer with a free slot, keeps four fallbacks, downloads, tags it against \
                 MusicBrainz and files it into the library. It returns a job; poll `job(id)` until `status` is \
                 `imported`, `review` or `failed`. Downloads take minutes to hours depending on the peer, so report \
                 progress rather than waiting in a loop.\n\n\
                 A job in `review` downloaded fine but the tagger was not sure which release it is. Its `candidates` \
                 are MusicBrainz releases with a distance (0 is perfect); pick the right one, asking the user if it \
                 is not obvious, and call `resolveJob(id, releaseId)`. If no candidate fits these files (wrong \
                 edition, missing discs), `nextSource(id)` drops them and downloads the next copy found.\n\n\
                 A job in `suspect` downloaded fine but its spectrum says the \"lossless\" files came from a lossy \
                 source, or were upsampled; `error` says why and `analysis` has the per-track evidence. Tell the \
                 user, and prefer `nextSource(id)` (while `alternates` > 0) or grabbing again over `approveJob`, \
                 which imports it anyway.\n\n\
                 `search` returns folders grouped by user, best first, for when the user wants to choose. Searching \
                 waits several seconds for peers to answer.\n\n\
                 For something nobody has yet, `addWish(query, grab: true)` keeps searching on the server's \
                 wishlist interval and starts a job when it turns up.\n\n\
                 The client is also a chat client: rooms, private messages (`conversations`, `messages`), \
                 buddies and interests. `sendMessage` and `say` reach real people, so they take two calls: \
                 PREVIEW returns a token and sends nothing; show the user the preview, and only after they agree \
                 call CONFIRM with the token and the message unchanged. Other people's files and conversations \
                 are theirs: do not ban, message or cancel uploads unless asked.",
            )
    }
}
