# slsk-mcp

A Soulseek client that runs as a service: an async engine written for large
shares and heavy transfer loads, a web UI, and a GraphQL/MCP interface so an
assistant can search, download and import on your behalf.

| Crate | What |
|---|---|
| `slsk-proto` | The wire protocol. Pure encode/decode, no I/O. |
| `slsk-engine` | The client: server session, peers, transfers, shares, distributed search. |
| `slsk-mcp` | The service: jobs, import, GraphQL, MCP, web UI. |
