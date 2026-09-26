# Changelog

## 0.1.19

- Every outcome a job reaches is recorded with a cause and the version that produced it (`job_events`), and kept after the job is retried or removed. The `triage` query groups them by cause with recent examples, and `slsk_job_outcomes_total{outcome,cause}` counts them: a cause that keeps recurring is a fix to make, and the version says which jobs to repair once it is made.
- The UI and MCP keep serving while a stopping pod finishes its import. The listeners closed at the stop signal, so a deploy during an import left the UI unreachable for as long as the import ran.

## 0.1.18

- A copy whose files will not parse (a truncated or corrupt FLAC) moves to the next source by itself. It stopped as `failed`, waiting for someone to ask for another copy, though the fault was the copy's and not the album's.
- Compilations tagged `VA` are matched as "Various Artists" (sift).

## 0.1.17

- `/metrics` moves to a port of its own (`METRICS_ADDR`, default 9464). The chart's NetworkPolicy had admitted the scraper to the internal port on the assumption it ran on the host network; it runs as a pod, so every scrape was refused and the dashboards were empty. Admitting it to the internal port instead would have let the metrics agent send credentials the service trusts, so it is admitted to the metrics port alone.
- The internal port admits the MCP gateway and nothing else. It had also admitted the whole home network, for a host-network scraper that does not exist.
- Grab ranks last the folders of a peer that already holds downloads of ours in its queue and is sending none, rather than adding to a queue that is not moving.
- The chart takes `scraperPodLabels` (default `app: metrics-vmagent`) for the pods allowed to scrape.

## 0.1.16

- Albums are grouped by what they need: **Needs you** (review, suspect, failed) first, then **On the way**, then **In your library** as one line each, showing the artist and album as filed rather than the query. The file-by-file transfer tables are folded away; they restate the album cards at a level only useful for diagnosis.
- Each card says in words what it needs ("Not sure which release this is…"), with the tagger's or analyser's reason in small print beneath and every action in one row at its foot. A note that an import will be tried again is no longer styled as an error.
- **Try another copy** (`nextSource`) drops the files a job holds and downloads the next folder found for the same request, for a transcode or a rip no release fits. Retry on a review job, which re-runs the tagger over the same files, is now labelled **Match again**.
- An album being imported cannot be removed, from the UI or the API: deleting its folder partway would leave it half in the library.
- An open spectral analysis stays open while the page updates.

## 0.1.15

- Long tracks get a length allowance in proportion to their length, so a twenty-minute side that differs by twenty seconds between editions still matches (sift).
- The import log shows what the best match's distance is made of: album, artist, titles and lengths (sift).

## 0.1.14

- A download that receives nothing for twenty minutes moves to its next source. Some peers queue every file and never send one (no free slot for strangers, or a queue they never work through), which held albums indefinitely while four other sources sat untried. A slow peer that is sending is left alone.

## 0.1.13

- Hi-res files are judged upsampled by whether anything sits above 26 kHz, and genuine when something does. The edge-steepness rule missed resamplers with a gentle filter and flagged dark tracks on genuine 24/96 releases; on known transcodes the new measure separates the two by about 30 dB.
- Every button shows that its request is in flight, and for exactly as long: the album's card and its buttons are marked busy and disabled from Datastar's request indicator, which also stops a second tap. Errors float at the foot of the screen, where they are seen, and clear on the next success.
- Use and Import anyway mark the album `importing` before they return, and import in the background. Use returned with nothing changed, which read as a tap that did nothing and invited a second one.
- A job waiting on a decision moves to `importing` in one database statement, so two requests for it (from the UI, GraphQL or MCP) cannot both start an import.
- Grab prefers FLAC to WAV unless the WAV is much sooner to arrive.

## 0.1.12

- Stopping waits for an import in progress (the chart allows ten minutes). Moving an album into the library is not atomic, and a restart partway through left it split between the staging folder and the library. Imports still queued stay `importing` and resume at the next start.
- An import that finds its job already imported does nothing. Imports queue on one lock, so a second request (a double tap, or Retry beside Use) ran after the first had filed the album and marked the job failed.
- A folder holding an album in two formats, or with `(1)` duplicates, imports one copy of each track, FLAC first (sift).
- `slsk-mcp analyse FILE…` prints the transcode analysis of any files as JSON lines.

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
