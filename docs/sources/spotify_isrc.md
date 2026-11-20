# `spotify_isrc`: another service's track or album on Spotify, by its ISRC or barcode

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`spotify_isrc` (on by default; it runs only with the operator's Spotify app credentials, `[spotify]
client_id` and `client_secret` or `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`;
`[also_on.sources.spotify_isrc] enabled = false` turns it off): for a track on another service
with an `isrc:` key (TIDAL's or Deezer's own record, a vouching page's, a hub's), the Spotify
tracks that carry exactly that ISRC, from the Web API's search with its `isrc:` filter
(`/v1/search?q=isrc:…&type=track`, an exact lookup, never a title), as links
`https://open.spotify.com/track/{id}` keyed by the ISRC. The track's own ISRC is asked first,
then the hubs', two at most; only an ISRC that is a key (a shared one is a fact, and Spotify would
answer with any of its recordings). Several answers are the recording on several releases:
[`editions`](editions.md) places them and a card shows one per service.

For an album on another service with a `upc:` key, the Spotify album that carries exactly that
barcode: the search's `upc:` filter (`/v1/search?q=upc:…&type=album&limit=3`; a UPC-A, a GTIN-13
with a leading zero, is asked by its 12 digits, an EAN-13 whole) names up to 3 candidates, and as
the search's album objects carry no barcode, each is read back with `/v1/albums/{id}` (the request
`spotify` makes for a Spotify album, shared with it; Spotify answers no batch read such as
`/v1/albums?ids=` to an app in development mode, per its February 2026 migration guide): only an
album whose own `external_ids.upc` (as GTIN-13) is the key counts. Exactly one such album becomes
a link `https://open.spotify.com/album/{id}` keyed by the barcode; none or several give nothing,
and so does a search that found more albums than it lists (a `next` page, or a `total` above its
items), which may hide another album with the barcode. Two barcode keys at most.

No keys of the resource and no facts. Spotify's own tracks and albums are not asked. It runs after
every source that keys a track's ISRC or an album's barcode (`deezer` among them).

Spotify answers an app in development mode only while its owner has an active Premium
subscription; else every request is refused (403): the refusal is held five minutes
(`spotify_auth`) and the source is passed over meanwhile, as for `spotify`.

## Operations

`spotify_isrc` costs one search request per ISRC key of a track on another service (two at most),
and per barcode key of an album one search and one read per album it lists (three at most),
shared for an hour by credential pair, on the same pace as `spotify` (2 requests/s on
`api.spotify.com`), through the same token slot: a refusal of the credentials holds five minutes
(`spotify_auth`), a rate limit or an outage is waited out (`spotify_busy`), and either keeps the
links an earlier run found. A 403 on one album's read is that read's, not the credentials': the
lookup is retried after five minutes (`forbidden_request`), keeping its links, and the token slot
stays open for every other Spotify request. Switched off, the rows it stored stop showing on cards
and expire as usual; after an upgrade that adds or changes it, `pinakesd reextract also-on` asks
for every stored track and album at once. The credentials and the token never appear in logs, the
operator info, errors or debug output. The shared Spotify client is `also_on/spotify_api.rs`; `spotify` and `spotify_isrc` each
go without the other.
