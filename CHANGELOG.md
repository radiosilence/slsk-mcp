# Changelog

## 0.1.11

- Failed files are asked for again from the same peer, after a minute and then two more, before another source is tried. The fallback started the album again from nothing, so one dropped connection late in a download threw away everything already fetched; retries resume from the partial files. The errors behind a fallback are logged.
- An album is held as suspect only when at least a quarter of its tracks confidently look lossy or upsampled (or most do at all). Albums are transcoded whole, so one odd track among clean ones is far more often a quiet or band-limited master.

## 0.1.10

- Grab ranks folders by how well they answer the query, not only by quality. Peers match words anywhere in a path, so any folder under an artist's directory used to qualify: a query's words must now each appear as often as the query repeats them (a self-titled album needs the name twice), and live, demo, B-side and remix folders rank behind the album unless the query names them.
- An album split into `CD 1`, `CD 2` folders is grabbed as one job, each disc in its own subdirectory, rather than as whichever disc ranked first.
- Download progress bars show real progress. They set their width inline, which the content security policy blocks, so every bar drew full; they are now `<progress>` elements. A job reads "waiting for the peer" until its first byte arrives.
- The album list logs why it failed to load instead of rendering empty.

## 0.1.9

- MusicBrainz pacing adapts to the service's global budget, and server errors, timeouts and dropped connections are retried as beets retries them (sift).
- An import that MusicBrainz cannot serve stays `importing` and is tried again every five minutes, rather than failing and waiting for someone to press Retry.
- Albums with many pressings match the one with the folder's track count, rather than whichever MusicBrainz lists first (sift).
- Remove asks first when the album has not been imported, since it deletes the downloaded files.
- A download that lands where an earlier attempt left the same album replaces it, rather than being imported from the old copy and left behind in `incomplete/`.

## 0.1.8

- MusicBrainz responses are kept in the state directory — releases for a week, searches for a day — so retrying an import, or importing another copy of an album, asks MusicBrainz for nothing it has already answered (sift).

## 0.1.7

- MusicBrainz rate limiting is waited out rather than failing the import: up to ten attempts, honouring `Retry-After`, with the request gate held meanwhile (sift).
- Titles are searched and compared without edition markers, so "Monster (25th Anniversary Edition)" finds "Monster" (sift).
- Retry on an album that failed at import imports it again rather than downloading it again, and runs in the background.
- Transfer states read short on a phone: `queued #4`, `active`, `done`.

## 0.1.6

- Imports are renames again. The chart mounted the library and the download area as two volumes, and a rename cannot cross mounts even on one disk, so every import copied the album — minutes for a hi-res record on a USB drive. It now mounts the drive once (`mediaRoot`), with `library` and `downloads` inside it; the service refuses to start while the library is missing, which is how an unmounted drive shows.
- The background tag pass reads with two threads rather than one per core: on a single spinning disk more readers only add seeks, and imports and uploads share the disk.
- The web UI fits a phone: rows wrap, long names wrap rather than widening the page, and inputs no longer make iOS zoom.

## 0.1.5

- Share the whole library within seconds of starting. A cold probe cache used to mean reading every file's headers before announcing anything — about an hour and a half for fifty thousand files on a USB disk, sharing nothing meanwhile. Now a walk shares everything at once, from the cache where it knows the file and without audio attributes where it does not, and a second pass fills those in, saving the cache every two thousand files so a restart keeps its progress.

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
