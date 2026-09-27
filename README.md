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
   user (disc folders such as `CD 1` count as their album), keeps those whose
   path mentions every word of the query as often as the query does, puts
   live, demo and remix folders behind the album unless asked for, and ranks
   lossless first (FLAC before WAV), then free upload slot, queue length and
   speed.
2. The best folder's full listing is requested from the peer and each file is
   queued. Failed files are asked for again from the same peer twice, resuming
   from what arrived; after that, or after twenty minutes without a byte, the
   next of four fallback folders is tried.
3. Every lossless-labelled track is checked for a lossy or upsampled source
   (see below). An album where a quarter of the tracks confidently fail is
   held as `suspect` with per-track spectrograms, for a person or assistant to
   import anyway or replace.
4. When every file has arrived, sift matches the folder against MusicBrainz.
   A complete match below the distance threshold is tagged, given cover art
   and moved into the library by the configured template. Anything less
   certain stops in `review` with candidates for a person — or the assistant —
   to choose from, or, for a release MusicBrainz lacks, to import as-is by the
   files' own tags when those describe one album.

### The transcode check

A lossy encoder low-passes its input, so a FLAC made from an MP3 has a
spectrum that stops dead below 20.5 kHz where a CD master runs to 21–22 kHz; a
gradual roll-off is read as a dark master, not a codec, and never holds an
album. A hi-res file is judged by whether anything at all sits above 26 kHz:
real recordings carry at least tape or microphone noise there, and a file
resampled up from 44.1 or 48 kHz carries none. A 24-bit file using only 16
bits is padded.

Measured on tracks from several albums (quiet, noisy and lo-fi material
among them) transcoded and decoded back to FLAC: no original was flagged; MP3
at CBR 128–320 kbps and `-V2`, padded 24-bit and upsampled 96 kHz files were
caught on every track. MP3 `-V0` from LAME 4 and AAC at 256 kbps keep the full
band and are not detected
([#8](https://github.com/radiosilence/slsk-mcp/issues/8)).

`slsk-mcp analyse FILE…` prints the check's reading of any files.

### When something goes wrong

Every outcome other than a clean import is recorded with a cause from a fixed
set (`stalled_peer`, `corrupt_copy`, `no_audio`, `no_candidates`,
`weak_match`, `incomplete`, `lossy_source`, `upsampled`, …) and the version
that produced it, in a history kept after the job itself is gone. The
`triage` query groups them by cause with examples, and
`slsk_job_outcomes_total` counts them for the dashboard. A cause that recurs
is a fix to make in code; the version then identifies the jobs to repair.

### The library after import

sift keeps an index of the whole library, derived from the files and rebuilt
incrementally before each library query, so the service can answer questions
about what is already there: which albums are held twice, and which are not
where the current naming rules put them (a library filed over years by
different beets configurations disagrees with itself). `duplicates` and
`refilePlan` only report; `binDuplicates` and `refile` act. Spare copies go to
`<library>-bin`, beside the library rather than in it, so Navidrome and the
shares stop seeing them and restoring one is a move back. Nothing deletes a
file.

Each import is followed by what beets' `replaygain`, `lastgenre` and `lyrics`
plugins add: loudness normalisation for players, genres from MusicBrainz, and
synced lyrics from LRCLIB. `enrichLibrary` backfills albums imported before.

## Running it

It is a long-running daemon: it holds one Soulseek login, shares the library
continuously, and serves the UI, MCP and metrics on three ports. Each of these works;
pick by what the host already runs.

- **Container** — `ghcr.io/radiosilence/slsk-mcp`, a static binary on
  `scratch`, for amd64 and arm64. `deploy/compose/docker-compose.yml` runs it
  with Postgres.
- **systemd** — the same binary is attached to each release;
  `deploy/systemd/slsk-mcp.service` runs it confined to the library and
  download directories.
- **Kubernetes** — `deploy/pulumi` is a Pulumi component
  (`@radiosilence/slsk-mcp-pulumi`, at the image's version): local volumes for
  the library and downloads pinned to the node that holds them, Postgres in
  the pod, a UPnP mapper for the peer port, and a NetworkPolicy confining it.

Whatever runs it, three things matter:

1. **The peer port** (`LISTEN_PORT`, TCP) must be reachable from the
   internet. Two peers both behind NAT cannot connect at all, so an
   unreachable client can only download from the half of the network that is
   reachable, and uploads to the other half never happen.
2. **The internal port** (`INTERNAL_ADDR`) trusts `X-Slsk-*` credential
   headers. Bind it to loopback or firewall it to the MCP gateway; never
   publish it. Metrics have a port of their own (`METRICS_ADDR`) so the
   scraper never needs this one.
3. **The library and downloads on one filesystem**, so an import is a rename
   rather than a copy of every album.

Open files: every peer connection is a socket, and a well-connected client
holds hundreds. Raise `LimitNOFILE`/`ulimit -n` above the default 1024.

## Monitoring

`/metrics` on the metrics port is Prometheus text: bytes up and down, uploads
and downloads by state, the upload queue, distinct users served, searches
received, answered and shed under load, distributed-network position, shared
files, folders and bytes, jobs by status, unread messages and open wishes.
Everything is a counter or a gauge with a small, fixed label set.

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
| `UI_ADDR`, `INTERNAL_ADDR`, `METRICS_ADDR`, `PUBLIC_URL` | `0.0.0.0:8080`, `0.0.0.0:8081`, `0.0.0.0:9464` | |

## Three listeners

The UI listens on one port behind OIDC. MCP and GraphQL listen on another,
where `X-Slsk-Username`/`X-Slsk-Password` headers are trusted without
question — that is how the MCP gateway passes the signed-in user's account —
so it must be reachable from the gateway only. `/metrics` has a third port
to itself, so a scraper can be admitted without being admitted to the second.
The chart's NetworkPolicy admits the gateway to one and the metrics agent to
the other, and nothing else to either.

## The web UI

The UI reaches everything GraphQL and MCP do, so a phone is a full client:
albums and uploads, then the wishlist, the library (duplicates, re-filing,
enrichment), browsing a user's shares, rooms and private messages, bans,
settings and triage. Each action calls the same Rust the GraphQL resolvers
call. Buttons show busy while their request is in flight and then the
server's answer, never a guess. Anything that changes the library or reaches
other people (binning, re-filing, enriching, banning, sending a message) asks
first.

Re-filing groups albums by what would change. File names, `_`/`-` folder
spelling and the folder's year only correct how an album is written, so they
can be applied together; an album whose tags name a different artist or
album than its folder is moved one at a time, after a look. Library actions
name albums by exact path, so they act only on what was shown. Enrichment
runs in the background, an album at a time, with progress on the page, since
it takes seconds per album.

## Security notes

- Every UI route except sign-in, the probe and static assets is behind the
  session layer, which wraps the router whole.
- State-changing UI requests must carry Datastar's request header, which a
  cross-site form cannot set.
- Peer-chosen strings (user, room and file names, messages) reach the server
  in form fields, and folders as base64 keys, never interpolated into a
  Datastar expression. The CSP forbids inline scripts and styles.
- Sending a message takes the same preview and confirmation tokens as the
  API; the Send button, after a confirmation, is what confirms.
- File names from peers never leave a job's staging directory, and the
  importer refuses paths that would leave the library.
- Credentials are sealed with XChaCha20-Poly1305 in the database and held in
  memory only as long as the session needs them to log back in.

## Load

`cargo test --release -p slsk-engine --test load -- --ignored --nocapture`
queues 40,000 transfers at once in each direction through real sockets: one
client downloading 1,000 files from each of 40 peers, and one library serving
1,000 files to each of 40 clients, every byte checked on arrival
(`LOAD_PEERS`, `LOAD_FILES` scale it). On a laptop each run completes in about
two minutes with no failures. Throughput there is set by the protocol's
per-file handshakes and one upload per user at a time, not by the engine.

## Development

```
cargo test --workspace
```

The network tests run real engines against `slsk-testserver` over loopback.
To run the service locally: Postgres, a `SEAL_KEY`, and
`UI_INSECURE_NO_AUTH=1 UI_ADDR=127.0.0.1:8080`.
