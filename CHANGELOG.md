# Changelog

## 0.1.53

- **Library queries no longer walk the library.** `libraryAlbums`, `duplicates`, `refilePlan` and the Library page each statted every file before answering, holding the one index lock while they did, so on a large library the page took minutes and requests queued behind one another. Reads now come straight from the index, on a connection of their own. The index is refreshed in the background at startup, after each import and every 15 minutes; changes still refresh before acting. Enriching the library album by album no longer re-reads the whole library per album.
- **Postgres indexes for the job list, unread counts, triage and wishes.**

## 0.1.52

- **`grab(refetch: true)` replaces the filed copy** (sift 0.3.4). A refetched album that finished downloading while the bad copy was still in the library was taken for a repeat of it (same folder, format and stated lengths), marked imported, and deleted, leaving only the bad copy. A refetched job now imports with `import_replacing`: the folder already there moves to the bin beside the library and the new copy is filed. The bad copy no longer needs moving out by hand first.

## 0.1.51

- **Searches are spaced at least 4 seconds apart.** Two dozen grabs inside a minute each searched at once, and the Soulseek server banned the account for 30 minutes for flooding. Every search (grabs, retries, wishlist, the Search page) now waits for its slot, so a burst of requests reaches the server as a steady trickle; a grab's listening window starts when its search goes out.
- **A ban is waited out.** When the server announces "You have been banned for N minutes", the client stops logging in until a minute after it lifts. It had kept retrying on its usual backoff, reaching every two minutes, which the server counts against the account.

## 0.1.50

- **`duplicates` pairs one album tagged two ways** (sift 0.3.3, [#15](https://github.com/radiosilence/slsk-mcp/issues/15)). The same album under two album-artist spellings, or with track titles spelled differently, is paired when the album title and track count match and each track is within 3 seconds in length and closely titled. A spare paired only this way reads "the same album, tagged differently".

## 0.1.49

- **An import runs to the end even when whoever asked for it stops waiting** ([#13](https://github.com/radiosilence/slsk-mcp/issues/13)). `resolveJob`, `approveJob`, `importAsIs` and the page's as-is button ran the import inside the request, so a dropped request cancelled it partway: sift's file moves carried on in blocking threads and filed the album, but the job never recorded it, and the next start failed it with "no audio files". Imports now run in their own task, which holds the import lock until the job is recorded, so shutdown waits for them too. Each import logs its start and outcome with the job id.

## 0.1.48

- **Imports file under the artist folder already there when names differ only in case** (sift 0.3.2). "The Squire Of Gothos" and "The Squire of Gothos" no longer make two folders; each part of the destination takes the spelling already on disk.

## 0.1.47

- **`pendingChecks`: peers holding downloads behind a message the service could not answer.** When a check is not in the standard form, an assistant can see it, with the peer's words marked as untrusted data. The MCP instructions confine it to answering the check with a short literal reply through `sendMessage`'s preview and confirm, which the user sees before anything is sent, and to doing nothing else the message asks.

## 0.1.46

- **Peers' download checks are answered automatically.** Some sharers hold every download until the requester types a code back ("please type \"ABBCCC\" in this chat"). The service now replies with the code itself when a message is plainly that check (a 3–16 character letters-and-digits token between quotes, asked to be typed "in this chat", no link), comes from a peer we have downloads queued or running with, and that peer has not been answered in the last day. The decision is made by code, not a model: nothing else in the message is read, and the token goes back only to the peer who asked. Each reply is logged and appears in the conversation.

## 0.1.45

- `grab(refetch: true)` always fetches a new copy, skipping the check that hands back an earlier job for the same query: for replacing a copy that turned out bad while the old one is still in the library.

## 0.1.44

- A repeated `grab` returns the earlier job only while what that job imported is still in the library. Once the album has been binned (a spare, or a damaged copy), asking again fetches a new copy instead of handing back the old job.

## 0.1.43

- **Downloads are no longer spliced into damaged files.** Resuming a stalled transfer asks the peer to continue from what is already on disk; some peers send the whole file from the start regardless, and the engine appended that, producing a file of two copies joined together that plays as noise. Albums grabbed twice at once made this common, since the peer then served each file twice. The engine now compares the first bytes a resumed transfer sends with the start of the partial file, and starts the file over when they match.
- **A copy that does not decode is never imported.** The pre-import analysis already decoded every file end to end, but skipped frames it could not decode without counting them. It counts them now (`decodeErrors` in a job's `analysis`), and a job with any damaged file fails with cause `corrupt_copy` and moves to the next source, whoever approved it.

## 0.1.42

- `modifyAlbums(query, changes)` corrects tags on albums already in the library, with beets' `field=value` and `field!`, and re-files any the change moves; a move onto an existing album is refused and reported. Until now tags could only be settled while a job was in review. Backed by sift's `modify`.

## 0.1.41

- A repeated `grab` returns the job the first one made. `grab` searches before it answers, which can outlast an MCP connector's timeout, and the retry started a second download of the same album. Grabs for one query now take turns, and each returns a job for that query made in the last day unless it failed.

## 0.1.40

- Lifetime totals that survive restarts: `slsk_lifetime_uploaded_bytes_total`, `slsk_lifetime_downloaded_bytes_total` and `slsk_lifetime_uploads_total{state}`, kept in a `totals` table. The engine's own counters start from zero with each process, and the upload history they could otherwise be read from is pruned after a few weeks.
- `slsk_served_users{window="24h"|"7d"|"all"}`: distinct users an upload has finished to, from a `served_users` table with each user's first and last upload. `slsk_upload_users` counted since the process started, so it read as zero after every deploy.
- The migration seeds both from the history still held.

## 0.1.39

- sift 0.3.1: Discogs is consulted when MusicBrainz has no strong match, given a token. The deploy package takes `discogsToken` and passes it to the service as `DISCOGS_TOKEN`.

## 0.1.38

- sift 0.2.0: imports now move a featured artist into the title (`ftintitle`), keep files' own dates as their added time and across moves (`importadded`), fill a missing year from MusicBrainz's release group (`yearfixer`), and honour `fetchart`'s minimum width, quality, aspect-ratio and high-resolution options, resizing art above the maximum width. Each follows the plugins listed in the beets config the service is given.
- rmcp 3.

## 0.1.37

- A job whose album is already in the library, as another copy in the same format, ends as imported and points at the album there, and the new copy is dropped. It failed with "… is already in the library" and offered another copy, which would have been refused the same way.

## 0.1.36

- A job with no stored fallbacks searches for another copy when its peer refuses or keeps failing, as a stalled job already did. A folder picked by hand has no fallbacks, so a peer's daily file limit ("Too many files today") or a ban failed it outright although other peers had the album.
- The web UI has an icon: the Soulseek bird, painted in kōan's style. It is the favicon and the home-screen icon.

## 0.1.35

- Browse: Download folder is offered at every level, not only in a folder holding files. Everything under the folder is downloaded as one job per album, with disc folders kept inside their album; above 100 files it asks first, and more than 200 albums is refused as a whole collection.
- Chat: sending no longer asks for confirmation in the web UI. The MCP keeps its preview and confirm steps.

## 0.1.33

- The web UI covers everything GraphQL and MCP do, in new tabs beside Albums and Uploads; the tab bar scrolls sideways on a phone.
  - Wishlist: add (lossless, grab when found), remove, when each was last searched, and the album it became, linked to its card.
  - Library: find albums by beets query; duplicates with the copy kept and why the others are spare, binned per album or all at once; the re-file plan grouped by what would change (file names, folder spelling, year, tags disagreeing with the folder, refused), applied per album or for every safe group at once; enrichment per album, per artist or for a query, run in the background with progress and each album's result.
  - Browse: a user's shares folder by folder, with the user's info, and download of a folder or of ticked files. Reached from a search result (opening at that folder), an uploading peer, a conversation or a buddy. The last few listings are kept for ten minutes.
  - Chat: joined rooms, public rooms to join, private conversations with unread counts (also on the tab), buddies with status, and a conversation view refreshed every few seconds while open. Sending uses the API's preview and confirm tokens.
  - Bans, list and add or remove; Settings: upload slots, speed limits, rescan, reconnect, and interests; Triage: why albums did not land, by cause, over 7, 30 or 90 days, with recent examples.
- Downloads started from the UI and bans confirm themselves in a message that fades; errors can be tapped away.
- The engine reports its current upload slots and speed limits.
- Removing an album that a wishlist entry grabbed removes the wish too. The wish had forgotten its album when the job went, and grabbed the same copy again on its next pass, so a removed album kept coming back.

## 0.1.32

- The uploads page keeps a history: every finished upload (who, what, how much, average speed while sending, and whether it completed), with today's and this week's totals. Recorded in Postgres as each upload ends and kept for 180 days, so it survives restarts, where the engine's own list holds only recent transfers in memory.
- The MCP guidance mentions `enrichLibrary` and that it should run an artist or album at a time.

## 0.1.31

- Every imported album is given ReplayGain track and album gain, MusicBrainz genres when it has none, and time-synced lyrics from LRCLIB where they exist, as beets' `replaygain`, `lastgenre` and `lyrics` plugins would (sift's `Importer::enrich`). It runs after the album is filed, so the album is playable first; a failure is logged, not a failed import.
- `enrichLibrary(query)` does the same for albums already in the library. It writes only those tags, but across everything the query matches, so it requires a query.

## 0.1.30

- Albums whose files number tracks straight through (B-side files tagged disc 2, tracks 5–8) match a release on one medium, or on sides that restart at 1, by overall position (sift). Rezzett's LP had matched with four tracks "missing" and the same four "extra".

## 0.1.29

- Searching again for a stalled album accepts only a copy as good as the one it replaces: lossless for a lossless album, and at least four-fifths of its tracks. It had replaced a 21-file FLAC Boiler Room set with a one-file 192 kbps video rip, and a twelve-track album with a single track from it.
- A rip numbering its tracks straight through a vinyl release whose sides restart at 1 matches every track instead of only the first side's (sift).

## 0.1.28

- The library as a whole is reachable over GraphQL and MCP, through sift's index of it: `libraryAlbums(query)` lists albums with beets' query syntax; `duplicates` finds albums held more than once and names the copy to keep (lossless over lossy, then more tracks, then higher resolution, then the one filed under the current rules); `binDuplicates` moves the spare copies to `<library>-bin`; `refilePlan` and `refile` move albums to where the current naming rules put them. The bin is outside what Navidrome and the shares see, and on the same drive, so binning is a rename; nothing is deleted. `refile` refuses an empty query, so re-filing the whole library is deliberate (`[""]`).
- The index lives in the state directory, is built at startup and brought up to date incrementally before each library query. A scan reads two files at a time, since a file the tag reader cannot parse can cost tens of MB while it tries, and such files are not re-read until they change.
- Filing follows beets' placement of the edge replacements (`^\.`, `\.$`): they apply to each path component rather than to every value inside it, so "The Vertigo E.P. [MP3]" keeps its dots (sift). ALAC is filed as `[ALAC]`, not `[AAC]` (sift).

## 0.1.27

- An album stalled on its only known copy searches again for its title, and moves to the best copy on another peer if one is online. Without this, a rare album found on one peer waited on that peer indefinitely. Searches are spaced a stall (twenty minutes) apart and skip peers that stalled recently.

## 0.1.26

- A download counts as stalled when no bytes have arrived for twenty minutes, rather than when none ever have. A peer that sent the first file and queued the rest behind hundreds of others was treated as sending, so the album never moved to another copy.
- A download waiting in the peer's upload queue shows its place ("place 535 in larsinio's queue") instead of 0%.
- UI sign-ins are kept in Postgres, so a deploy no longer signs everyone out. Only the SHA-256 of each session id is stored.

## 0.1.25

- Grab ranks a folder named close to the query ahead of one carrying much else in its name: a leaf folder with more than three words beyond the query is treated like a variant. Scene-style release names ("Artist-Album-(CAT001)-WEB-FLAC-2019-GROUP") had ranked alongside the plain album folder.
- Filing an album that is already filed, identically, succeeds rather than failing on existing files (sift). A retried import after a crash or double submission no longer ends in an error; a partial overlap with different files is still refused.

## 0.1.24

- Each album waiting on a decision leads with one suggested action and the reason, the judgement otherwise made from the evidence on the card: a release that lines up track for track is **Use this release**; a release MusicBrainz lacks, with complete tags, is **Import as-is**; a copy with missing tracks, untagged files or lossy audio is **Try another copy**; audio only padded to 24-bit is **Import anyway**. Other actions stay available, less prominently.
- Import as-is is checked when an album reaches review (`asIsBlocker`), and is offered only when it would be accepted; otherwise the card says why. When it is refused, the refusal is shown, rather than the card quietly returning to where it was.
- Statuses read as a person would say them: needs a choice, check quality, filing, in library.
- A fallback puts peers that stalled recently last, as grab does; the order was fixed when the job began.
- `cargo test --release -p slsk-engine --test load -- --ignored` runs 40,000 downloads from 40 peers and 40,000 uploads to 40 clients through the test server, checking every byte. On a laptop both complete in about two minutes with no failures, the engines together peaking at 360–615 MB.

## 0.1.23

- An assistant can work the review queue end to end. `jobTracks(id)` gives a job's files and their tags; `compareRelease(id, releaseId)` lines them up against a release track by track, with title and length differences and missing and extra tracks; `importAsIs(id, edits)` corrects album, artist, title or track numbers before filing, under the same coherence check as the files' own tags. The MCP guidance describes when each applies (sift).

## 0.1.22

- Fallback sources keep the file list the search found, and use it when the peer will not list the folder, as the first choice already did. A fallback kept only its folder name, so a peer that did not answer a folder listing was no fallback at all.
- Stalls are not judged for three minutes after a start. The stall clock counts from the job row, so the first tick after a restart judged every overdue job before the session had logged in, and every fallback's folder listing failed with it: eight albums failed with four untried copies each.
- A peer that stalls a download is remembered for a day, across restarts, and grab ranks its folders last. Cancelling our queue with it made it look idle again, and it would be chosen again.
- Outcome history records the peer involved.
- An album whose every source failed says why ("the peer sent nothing for twenty minutes, and no other copy could be fetched") rather than "download failed".

## 0.1.21

- DJ-mix folders (`mix`, `mixed`, `fabric`, `podcast`) rank behind the album unless the query names them. "Artist - fabric 91: Artist" repeats the artist's name, so it passed for the self-titled album.

## 0.1.20

- **Import as-is** (`importAsIs`) files an album in review by its files' own tags, for a release MusicBrainz does not have. It is refused, and the album stays in review with the reason, unless the tags describe one album (sift).
- An **Uploads** tab shows who is taking files from you, grouped by person, with progress, speed, cancel and ban; its badge counts files being sent or waiting to be. Downloads stay under Albums.
- The stall limit counts from when a job took on its current source, as the job records it, rather than from when the process started, so a restart no longer gives a peer that has sent nothing another twenty minutes. Repeated deploys had kept albums queued behind such a peer for an hour.

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
