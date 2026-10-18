Behaving well on the network, which also keeps the account clear of the server's flood bans.

- **Searches are rationed: 60 in any hour by default** (`SEARCHES_PER_HOUR`), on top of the four-second gap between them. Each search is relayed to thousands of peers, and an assistant left to itself rephrases and retries far more than a person would. A search while disconnected is refused without spending any of it. Past the budget a search is refused at once with when the next is free, rather than queued; the wishlist keeps its place in line.
- **The same words searched again within ten minutes reuse the earlier answers**, when the earlier search listened at least as long. This covers an assistant re-running a search, a `grab` after a `search`, and a stalled job looking for another copy.
- **A `grab` searches once.** When no copy is lossless, it re-reads the same answers without the filter instead of searching the network a second time.
- **A file the peer repeatedly fails to send now gives up** after the usual number of attempts, instead of being queued again with that peer indefinitely.
- **The default user info says what this is**: an automated client that shares its library. `DESCRIPTION` still replaces it.
