# `spotify_isrc`: another service's track or album on Spotify, by its ISRC or barcode

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`spotify_isrc` (**off by default**; on only with `[also_on.sources.spotify_isrc] enabled = true` and the
operator's Spotify app credentials, `[spotify] client_id` and `client_secret` or
`PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`): for a track on another service
with an `isrc:` key (TIDAL's or Deezer's own record, a vouching page's, a hub's), the Spotify
tracks that carry exactly that ISRC, from the Web API's search with its `isrc:` filter
(`/v1/search?q=isrc:…&type=track`, an exact lookup, never a title), as links
`https://open.spotify.com/track/{id}` keyed by the ISRC. The track's own ISRC is asked first,
then the hubs', two at most; only an ISRC that is a key (a shared one is a fact, and Spotify would
answer with any of its recordings). Several answers are the recording on several releases:
[`editions`](editions.md) places them and a card shows one per service.

For an album on another service with a `upc:` key, the Spotify album that carries exactly that
barcode: the search's `upc:` filter (`/v1/search?q=upc:…&type=album&limit=10`; a UPC-A, a GTIN-13
with a leading zero, is asked by its 12 digits, an EAN-13 whole) names up to 10 candidates, and as
the search's album objects carry no barcode, `/v1/albums?ids=…` reads them back: only an album
whose own `external_ids.upc` (as GTIN-13) is the key counts. Exactly one such album becomes a link
`https://open.spotify.com/album/{id}` keyed by the barcode; none or several give nothing. Two
barcode keys at most.

No keys of the resource and no facts. Spotify's own tracks and albums are not asked. It runs after
every source that keys a track's ISRC or an album's barcode (`deezer` among them).

## Why it is off by default

Spotify's Developer Policy (effective 15 May 2025) says "Do not create any product or service
which is integrated with streams or content from another service" (III.5), and an "Also on" list
puts a Spotify link beside other services' links. The same clause's shape in TIDAL's guidelines is
why there is no TIDAL lookup by ISRC at all. Turning this source on is the operator's reading of
those terms, and it needs an app Spotify answers: in development mode the app's owner needs an
active Premium subscription, else every request is refused (403) and the lookup is held five
minutes (`spotify_auth`), as for `spotify`.

Open point, not a claim of compliance: the Developer Terms (version 10, 15 May 2025) allow "the
temporary caching of: metadata and cover art" (IV.3.b) and forbid stored "compilations or databases
of Spotify Content, other than as strictly necessary to operate your SDA" (IV.3.a); a derived link
lives the source's lifetime (30 days, twice that in the table) like every derived link, and
whether that is "temporary" is for the operator to judge. The Policy also asks that metadata be
"accompanied by a link back to the applicable album, content or playlist on the Spotify Service"
(II.4.2); the link is all this source stores.

## Operations

`spotify_isrc` costs one search request per ISRC key of a track on another service (two at most),
and per barcode key of an album one search and, when it lists albums, one read of them,
shared for an hour by credential pair, on the same pace as `spotify` (2 requests/s on
`api.spotify.com`), through the same token slot: a refusal of the credentials holds five minutes
(`spotify_auth`), a rate limit or an outage is waited out (`spotify_busy`), and either keeps the
links an earlier run found. Switched off, the rows it stored stop showing on cards and expire as
usual; switched on after an upgrade, `pinakesd reextract also-on` asks for every stored track and
album at once. The credentials and the token never appear in logs, the operator info, errors or debug
output. The shared Spotify client is `also_on/spotify_api.rs`; `spotify` and `spotify_isrc` each
go without the other.
