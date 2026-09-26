# slsk-mcp

A Soulseek client that runs as a service. It shares a music library, fetches
albums into it, and is driven from a web UI or by an assistant over MCP:
"grab me Twoism" searches the network, downloads the best lossless copy,
matches it against MusicBrainz, tags it and files it into the library.

| Crate | What |
|---|---|
| `slsk-proto` | The wire protocol. Pure encode/decode over `bytes`, no I/O. |
| `slsk-engine` | The client: server session, peer connections, share index, transfers, distributed search. |
| `slsk-testserver` | An in-process server for tests. |
| `slsk-mcp` | The service: jobs, import (via [sift](https://github.com/radiosilence/sift)), GraphQL, MCP and the web UI. |

`deploy/pulumi` is the Pulumi component that deploys it, published as
`@radiosilence/slsk-mcp-pulumi` at the same version as the image.

## Why an engine of its own

Existing clients hold a thread per peer and per transfer, and answer each
search from the distributed network by scanning every shared path. On a
large library that is the part that falls over. This engine runs on one
tokio runtime with bounded queues between tasks, indexes shares by word so a
search intersects short posting lists, serves browse requests from one
pre-compressed response, and forwards distributed searches to children as
shared buffers. Memory follows active work.

It speaks the protocol as the Nicotine+ documentation describes it and is
tested for interoperability against the `soulseek-rs` client.

## How a request becomes an album

1. `grab(query)` searches for a few seconds, groups results into folders per
   user, keeps those whose path mentions every word of the query, and ranks
   lossless first, then free upload slot, queue length and speed.
2. The best folder's full listing is requested from the peer and each file is
   queued. The next four folders are kept as fallbacks if the peer fails.
3. When every file has arrived, sift matches the folder against MusicBrainz.
   A complete match below the distance threshold is tagged, given cover art
   and moved into the library by the configured template. Anything less
   certain stops in `review` with candidates for a person — or the assistant —
   to choose from.

## Configuration

Read from the environment at start; a missing or malformed value fails
startup.

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | — | Postgres. Jobs, sealed credentials, bans. |
| `SEAL_KEY` | — | 32 bytes, base64. Seals Soulseek credentials at rest. |
| `SLSK_USERNAME`, `SLSK_PASSWORD` | — | Log in at start. Credentials from the gateway or the UI replace them. |
| `LIBRARY_DIR` | `/music` | Where imports are filed. |
| `SHARE_DIRS` | the library | Comma-separated. |
| `STAGING_DIR` | `/data/incomplete` | Downloads in progress. |
| `COMPLETE_DIR` | `/data/complete` | Finished downloads waiting for import, or for a person when the tagger could not place them. Best on the library's filesystem, where an import is a rename. |
| `STATE_DIR` | `/data` | The share-probe cache. |
| `LISTEN_PORT` | `2234` | The peer port. Must be reachable for peers behind NAT to connect. |
| `UPLOAD_SLOTS`, `UPLOAD_LIMIT`, `DOWNLOAD_LIMIT` | `5`, `0`, `0` | Limits in bytes per second; 0 is unlimited. |
| `BEETS_CONFIG` | — | A beets `config.yaml` for the importer's template and replacements. |
| `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` | — | Required for the UI. `UI_INSECURE_NO_AUTH=1` disables sign-in and is only accepted with a loopback `UI_ADDR`. |
| `UI_ADDR`, `INTERNAL_ADDR`, `PUBLIC_URL` | `0.0.0.0:8080`, `0.0.0.0:8081` | |

## Two listeners

The UI listens on one port behind OIDC. MCP, GraphQL and `/metrics` listen on
another, where `X-Slsk-Username`/`X-Slsk-Password` headers are trusted
without question — that is how the MCP gateway passes the signed-in user's
account. That port must be reachable from the gateway and the metrics
scraper only; the chart's NetworkPolicy enforces it.

## Security notes

- Every UI route except sign-in, the probe and static assets is behind the
  session layer, which wraps the router whole.
- State-changing UI requests must carry Datastar's request header, which a
  cross-site form cannot set.
- File names from peers never leave a job's staging directory, and the
  importer refuses paths that would leave the library.
- Credentials are sealed with XChaCha20-Poly1305 in the database and held in
  memory only as long as the session needs them to log back in.

## Development

```
cargo test --workspace
```

The network tests run real engines against `slsk-testserver` over loopback.
To run the service locally: Postgres, a `SEAL_KEY`, and
`UI_INSECURE_NO_AUTH=1 UI_ADDR=127.0.0.1:8080`.
