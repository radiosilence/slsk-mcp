# Changelog

## 0.7.0

### Added

- **Bin any album from the library.** The album panel always offers Bin, so a superseded copy that duplicate detection misses can still be moved to the bin; albums that are not spare copies preview unticked with a warning.
- **Library filter for albums in more than one format.** Lists albums the library also holds in another format, such as an MP3 left behind by a FLAC upgrade, including the pairs duplicate detection misses because their track lists differ.
- **Albums from a download URL.** `importUrl(url)`, or the link field on the Albums page, fetches an http or https link such as a Bandcamp purchase, unpacks it if it is a zip, and imports it through the same analysis, matching and review as a Soulseek download, so a purchase is checked before it is filed. Only the host is logged or shown, since such links carry access tokens.

## 0.6.0

### Added

- **Per-track and multi-album tag edits for library albums.** `libraryTracks(query)` reads each matching album's file names and current tags, and `editTracks(albums)` applies album-wide and per-file corrections across one or more albums in one call. Every album and file is checked before anything is written, a failed write puts back the files already written, and each album is re-filed afterwards as `modifyAlbums` does. `preview: true` lists every file's change and where each album would go. The web UI's edit form uses the same path, so its edits gain the same checks.

### Fixed

- **A search whose task died no longer blocks identical searches.** A cached search that ended without finishing is replaced by a fresh one instead of answering "channel closed" for ten minutes and leaving the web UI on "Searching…".
- **`editTracks` tells files on separate discs apart and checks more before writing.** Files are keyed by their path within the album, so `CD1/01.flac` and `CD2/01.flac` no longer collide. A file that changed since it was read, or that cannot be opened for writing, stops the batch before anything is written. A failed write's error says what the undo could not restore. Albums are re-filed in one pass, and an album that cannot be re-filed is reported as refused rather than failing the call. The preview applies the same checks as the write and flags two albums headed for one folder. `libraryTracks` reads at most 50 albums.

### Internal

- **Workflows are linted.** CI runs actionlint, with shellcheck, over `.github/workflows`, and `just check` runs it locally.
- **UI tests live in one module.** The library and browse tests moved into `src/ui/tests.rs`, matching koan, so both projects keep their UI tests in the same place.

## 0.5.2

### Security

- **Dependencies updated for RustSec advisories.** Fixes rustls (RUSTSEC-2026-0346/0347/0348/0349), rustls-webpki (RUSTSEC-2026-0382) and webpki-root-certs (RUSTSEC-2026-0383).

### Internal

- **One CI/CD pipeline, gated by `Checks`.** `ci.yml` becomes `ci-cd.yml`. Jobs run only for the parts of the repository a change touches, and a single `Checks` job reports the result. The static binaries are built once; the image is assembled from them and pushed by commit, then retagged as `main` and, on a release, with its version once the checks pass.
- **A release run is no longer cancelled while it waits to move the image tag.** A concurrency group holds one pending job and cancels it when another queues, so release runs now wait in a group of their own.

## 0.5.1

### Fixed

- **Web UI styles on older browsers.** The compiled stylesheet no longer relies on CSS nesting, which Safari before 16.5, Chrome before 112 and Firefox before 117 ignore; on those, status labels were unreadable. A library address with a repeated filter key now lists the default view instead of failing.

## 0.5.0

### Added

- **Browsing shares works like Finder's list view.** Folders expand in place to any depth and the expanded set is kept in the address, every folder and file has its own download button, and this client's own shares can be browsed the same way.

### Changed

- **The web UI is styled with Tailwind v4.** Its stylesheet is compiled from utilities in the templates and design tokens named as in koan's web UI, so the two share one vocabulary. The look is unchanged apart from links underlining only on hover.

### Fixed

- **Web UI search has its own address and reuses cached searches.** Search moved to `/search?q=…`, where reloading, coming back or showing more joins a search of the same words from the last ten minutes instead of spending another; results are drawn a hundred folders at a time, and links to a user's shares open in a new tab.
- **Web UI assets are versioned.** The stylesheet and Datastar are linked by a hash of their contents and cached for good, so after an upgrade a browser fetches the new build's copies instead of reusing stale ones.

### Internal

- **Typed query parameters in the web UI.** Every page and fragment reads its address into a deserialized struct rather than picking fields out of raw pairs, so each page has one definition of the view state its URL carries.

## 0.4.1

### Changed

- **Every address is its own page.** The server draws each page whole around a shared layout instead of one page holding every tab, so a link or a reload costs one request and draws nothing for the other pages; the nav is plain links and the live stream updates only what the open page shows.

### Fixed

- **Jobs that need attention are capped too.** Failed, review and suspect jobs share the 500-row limit and "Show more" with settled ones, an index keeps listing them from reading every job ever made, and job buttons stay busy until their card is redrawn.

## 0.4.0

### Fixed

- **The web UI no longer crashes phone browsers.** Settled albums and upload history show the newest 500 rows with a "Show more" button, the live stream sends only the open tab's lists and only when they change, and a page opened on another tab no longer draws the album list first.

## 0.3.3

### Fixed

- Remove serverside generated transfer list, which was murdering browsers.

## 0.3.2

### Fixed

- Removed transfer file list from homescreen as they were absolutely huge and also pointles.

## 0.3.1

### Added

- **Library manager.** An album opens in detail with its files and tags; it can be re-tagged in place from a MusicBrainz release (picked or pasted) or edited by hand, each previewed field by field, old and new, with the move it causes. The list is no longer paged.

### Changed

- **Every list shows everything.** Jobs, downloads, uploads, search results, chat rooms and conversations, browse folders and triage examples are no longer cut off at a fixed number of rows.

### Fixed

- **Library fixes.** A re-imported album keeps its import date; binning skips a spare the bin already holds rather than stopping; the selection clears when the list is redrawn; a failed background run no longer blocks the next; "imported today" counts from local midnight; `reimportAlbums` previews the release it was given.

### Internal

- **The GitHub release is published with `RELEASE_TOKEN` only.** The fallback to the workflow token is gone now that the secret is set; a missing token fails the job by name, as in the other repositories.
- **sift from crates.io.** The Library tab's re-tag and edit previews take sift-music 0.8 from crates.io rather than a sift branch.

## 0.3.0

### Added

- **Previews over the API.** `binDuplicates`, `refile`, `creditArtists` and `enrichLibrary` take `preview: true` to return what they would write without writing it; `reimportAlbums` matches albums already filed against MusicBrainz again.

### Changed

- **Library tab is one list, every write previewed.** Albums are listed newest import first, filtered by a search, an import date, duplicates or albums the rules file elsewhere; ticked albums are re-filed, credited, enriched, re-imported or have spares binned, each shown album by album before anything changes, then applied in the background.

### Fixed

- **Navigation is made of links.** The tabs, the triage period, the folders in a search result or browse listing, and every control that opens a user's shares or a conversation are ordinary links to that place, so they can be opened in a new tab or copied. Triage keeps its period in the address. Folders in a search result path no longer stretch apart with wide gaps before each separator.

### Internal

- **Release script recuts a stale branch.** `scripts/release.sh` deletes a release branch whose PR was closed or merged, locally and on origin, and cuts it again from origin/main; a branch with an open PR still stops it.

## 0.2.0

### Added

- **Each artist on a collaboration is credited.** Imports tag every artist on a credit (`ARTISTS`, `ALBUMARTISTS` and every MusicBrainz artist id, through sift 0.6), so players list "A × B" under both; `creditArtists(query)` adds them to albums filed before.

### Fixed

- **Crediting holds up over many albums.** The web UI's enrich reads the library's artists once per run rather than once per album, and an unreadable album is reported instead of ending a `creditArtists` run.

### Internal

- **Releases are cut by `just release`, which opens a release PR.** Bumping the crate, the Pulumi package and `APP_VERSION` by hand on main let the three drift and gave no place to review the notes. CHANGELOG.md is now generated from fragments in `changelog.d/`, a push releases only when it changes the version, and the GitHub release takes its notes from the changelog.

## 0.1.68

- **A hidden track before track 1 is imported when the rip leaves it unnumbered.** MusicBrainz gives a pregap an estimated length that rips rarely share, and sift judged the track by it, so an untagged track 00 was left behind even when the release was chosen by hand, and a release missing the hidden track outscored the complete CD. sift 0.5.1 matches a pregap by title and position.

## 0.1.67

- **A CD's hidden track before track 1 is imported as track 0.** MusicBrainz lists it as the disc's pregap, apart from its tracks, and the tagger used to read only the tracks, so the rip's track 00 matched nothing and was deleted with the leftover download. Fixed in sift 0.5, which this release moves to.
- **The database layer uses rusqlite instead of sqlx**, as koan and sift do. sift 0.4 links SQLite through rusqlite 0.40, and sqlx 0.9 cannot share a binary with it, since each bundles its own SQLite. The stored encodings are unchanged (UUIDs as blobs, timestamps as RFC 3339 text with a `T`, JSON as text), and a database sqlx migrated opens as it is: its `_sqlx_migrations` record counts as applied, and `PRAGMA user_version` tracks migrations from here.

## 0.1.66

- **An album in review takes a pasted MusicBrainz release ID or URL.** The matcher's search sometimes misses a release MusicBrainz does have, which left only another copy or an as-is import, and as-is is refused when the files lack album tags.
- **The album search has a Clear button**, which empties the box and the results and drops the query from the address. It waits for a running search to finish, since the search would otherwise refill the results it cleared.

## 0.1.65

- **Browse lists every folder at a level.** It stopped at 500 with no way to reach the rest. A list of names is cheap to render even in the thousands, so the ceiling is now a million, only to bound a peer sending an absurd share list.

## 0.1.64

- **Searches come back quicker.** After a quiet spell, six searches go out a second apart before settling to one every four seconds, so a single search or a short run of them no longer waits on the four-second gap. A full burst followed by the steady rate stays under twenty in the first minute, below the flood that got the account banned. The burst refills after a quiet spell.
- **The hourly search budget defaults to 200** (`SEARCHES_PER_HOUR`), up from 60. The server publishes no hourly limit; its bans are for bursts and for repeating a search quickly, which the pacing and the ten-minute reuse already prevent.

## 0.1.63

Behaving well on the network, which also keeps the account clear of the server's flood bans.

- **Searches are rationed: 60 in any hour by default** (`SEARCHES_PER_HOUR`), on top of the four-second gap between them. Each search is relayed to thousands of peers, and an assistant left to itself rephrases and retries far more than a person would. A search while disconnected is refused without spending any of it. Past the budget a search is refused at once with when the next is free, rather than queued; the wishlist keeps its place in line.
- **The same words searched again within ten minutes reuse the earlier answers**, when the earlier search listened at least as long. This covers an assistant re-running a search, a `grab` after a `search`, and a stalled job looking for another copy.
- **A `grab` searches once.** When no copy is lossless, it re-reads the same answers without the filter instead of searching the network a second time.
- **A file the peer repeatedly fails to send now gives up** after the usual number of attempts, instead of being queued again with that peer indefinitely.
- **The default user info says what this is**: an automated client that shares its library. `DESCRIPTION` still replaces it.

## 0.1.62

- **Upload slots have no ceiling, and default to 500.** The Settings page and `setUploadSlots` held the count to 100 for no reason the engine has. Each running upload costs a socket and a 256 KiB buffer, and past what saturates the uplink more slots share it more ways rather than send more, so the number to pick follows the connection.

## 0.1.61

- **The address keeps your place, not only your tab.** An album search (`/?q=…`), a library search (`/library?q=…`), a user's shares and the folder in them (`/browse?username=…&key=…`) and an open conversation (`/chat?kind=pm&name=…`) are each in the address, so reloading, sharing the link, or going back and forward returns to the same place. Each tab also remembers where it was, so leaving a conversation for another tab and coming back reopens it.

## 0.1.60

- **Re-file shows exactly what it would do.** Each album on the Library page opens to the list of every file it would move, from and to, worked out from the files as they are when it is opened, including the cover and other files that follow the album.
- **Re-file what is ticked.** Albums are selected with a checkbox, or a whole group at once, and re-filed together; each row shows its outcome as it lands. Re-filing one album and then another no longer loses track of either: rows are found by the album they show, not their place in the list, and the page is redrawn once, when the last batch finishes. A batch reads the library once, not once per album.
- **Every tab has an address**: `/library`, `/chat`, `/uploads` and so on open on that tab, the address follows the tab, and back and forward move between tabs.

## 0.1.59

- **The Library page opens at once.** Planning where every album belongs takes seconds on a large library, and the page did it, with a second full read of the index, on every visit. The whole library's duplicates and re-file plan are now worked out once per change to the index, in the background right after each refresh, and served from there; `duplicates` and `refilePlan` without a query answer the same way. Re-filing still plans each album as it moves it, so a plan never acts on stale information. Planning also no longer runs on an async worker.

## 0.1.58

- **The web UI shows its version**, at the foot of every page, linked to that release's notes.

## 0.1.57

- **Postgres is gone.** 0.1.56 copied its state into SQLite; this release stops reading `DATABASE_URL`, and the Kubernetes component no longer runs a Postgres container. Its data directory (`statePath/postgres`) is left on disk to delete by hand. The component's `databasePassword` is still accepted, and ignored, so callers keep compiling until they drop it.
- **A file that cannot be read stops an import** ([#20](https://github.com/radiosilence/slsk-mcp/issues/20)). Analysis left out any track it could not open or decode at all, so the damaged-copy check never saw it and a broken file could be filed. Such a track is now recorded with the reason and counts as damage. A codec this build does not decode (AAC in an `.m4a`) is still skipped, not mistaken for damage.
- **Cancel and remove leave a job consistent** ([#21](https://github.com/radiosilence/slsk-mcp/issues/21)). `cancelJob` changed any job, an imported one included, to cancelled; it now cancels only a download in progress, and a deferred import of a cancelled job no longer runs later. `removeJob` takes the job out of every state an import starts from in one statement, so an import cannot start between its check and its delete, and it deletes the job's spectrograms.
- **Bookkeeping that only grew** ([#22](https://github.com/radiosilence/slsk-mcp/issues/22)). A queued download whose peer has gone is asked for again rather than waiting for good. A downloader is told when an upload to them fails before any bytes went, so they do not wait in a queue we have left. Peer addresses, unread searches, address requests that timed out, refused or cancelled upload queue entries and idle grab locks no longer accumulate. A peer that stops reading a browse or search reply loses the connection after 2 minutes instead of holding it.

## 0.1.56

- **State is kept in SQLite, not Postgres.** Jobs, sealed credentials, bans, messages, wishes, upload history and the lifetime totals are in `STATE_DIR/slsk.db`. One process is the only writer, so a database server added a process, a password and a network hop and took no load off. The file is opened once, at startup: reads share a pool of read-only connections, every write goes through one connection, and each connection runs on its own thread. The queries that run on every tick or page load are checked against SQLite's query plan in the tests and each is answered from an index.
- **Upgrading carries everything over.** With `DATABASE_URL` still set, the first start copies every table from the old Postgres into the new, empty `slsk.db`, then never reads Postgres again. Unset `DATABASE_URL` and stop the Postgres once it has started; the Kubernetes component keeps its Postgres for this release so the copy can happen, and drops it in the next.

## 0.1.55

- **Planning where albums go no longer lists the library per track** (sift 0.3.6). Matching a folder's existing spelling read the library root and statted every artist folder for each track, so `refilePlan`, `duplicates` alongside it and the Library page still took minutes after 0.1.53 stopped them walking the library. A folder of exactly the name is now checked with one stat, and a listing read only when that misses. Imports, which plan each track the same way, gain the same.

## 0.1.54

- **A file is no longer written by two transfers at once.** An uploader's "upload failed" during a transfer put the download back in the queue while the transfer was still writing its `.part` file; the uploader's next offer started a second one appending to the same file. A running transfer now ends on its own (its connection closes, or 2 minutes pass with no data) and is re-queued from there.
- **A peer that offers a file and never sends it is given up on** after the usual 8 attempts. Each 2-minute wait for its connection had sent the download back to the queue without counting, so it was asked forever. The offer's token is forgotten when it lapses.
- **A file connection must come from the uploader whose token it names.**

- **One job's error no longer stops every other job.** The download loop walked the jobs in one pass and any database error part-way through ended the pass, so every older job waited behind it, every two seconds, for as long as the error lasted. Each job is now followed on its own and its error logged against it. A job with no files to download, which nothing would ever move, now fails with that reason.
- **Job lists read every job's files in one query**, not one per job: the download loop every two seconds, the jobs panel (once a second per open tab) and `jobs(first: 500)`.
- **The chat list counts unread messages once per conversation**, not once per message.

- **A chat message could stop the chat.** The download-check reader found its place in a lowercased copy of the message and cut the original there; a character whose lowercase is longer (`İ`) shifted the offset, and one landing inside a character panicked the task that handles every server message, so chat, rooms and check answers stopped until restart. It now lowercases ASCII only, which keeps offsets.
- **Peers can no longer make us inflate what we did not ask for.** A share list nobody requested is dropped before it is inflated (it may expand to 256 MiB); search responses and folder listings inflate to at most 16 MiB. A frame's announced length is no longer reserved before its bytes arrive, so a peer that announces 128 MiB and sends nothing costs nothing.
- **Search requests are shed before the share index is searched**, not after, so load costs no index work. A `*tail` term alongside whole words filters their matches instead of scanning the whole vocabulary.
- **Browse and user-info requests that time out no longer leave their waiters behind.** Abandoned sign-ins expire from memory, and at most 1024 are kept.

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
