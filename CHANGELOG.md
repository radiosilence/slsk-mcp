# Changelog

## 0.1.4

- Start with a beets config that has no `directory`. A shared base config leaves it to a per-machine file, and the library comes from `LIBRARY_DIR` anyway; loading it refused, and the service exited at boot.

## 0.1.3

- Spectral analysis before import. Every lossless file is checked for where its spectrum ends and how abruptly; an album whose files confidently look like a lossy source or an upsample is held as `suspect` with per-track verdicts and spectrograms, rather than filed. `approveJob` imports it anyway.
- More in `/metrics`: login state, shared folders and bytes, distinct users served, transfers and jobs by state, distributed parent and depth, unread messages, open wishes.
- Deployment notes for containers, systemd and Kubernetes, with a Compose file and a unit.

## 0.1.2

- Rooms, private messages, buddies, a wishlist and interests, in GraphQL and so over MCP. Private messages are kept; rooms, watched users and interests are re-sent on every login, since the server forgets them with the session. The wishlist searches one entry per server-set interval and, with `grab`, starts a job for the first relevant folder. Messages to people take a PREVIEW and a CONFIRM.

## 0.1.1

- Downloads arrive in `incomplete/` and move to `complete/<album> [<id>]` when finished, where they wait for import — or, when the tagger cannot place them, for a person. On the library's drive that makes an import a rename, and leaves anything unresolved somewhere browsable (`COMPLETE_DIR`).
- The chart mounts the library and the download directory as local PersistentVolumes pinned to the media node by hostname, rather than hostPath volumes selected by label. Each volume sits on the directory itself, not the drive's mountpoint, so an unmounted drive leaves the pod waiting instead of writing to the root disk. `library` and `downloads` are paths; `node` replaces `nodeLabel`.

## 0.1.0

- The protocol crate, the async engine, and the service around it: jobs that carry a folder from a peer into the library through sift, a GraphQL API, an MCP server with `slsk_schema` and `slsk` tools, and a Datastar web UI.
- A Pulumi component that deploys the service with its Postgres, a UPnP mapper for the peer port, and a NetworkPolicy confining it to the internet, the gateway and Traefik.
