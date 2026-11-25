# `tidal_isrc`: another service's track or album on TIDAL, by its ISRC or barcode

This source runs on tracks and albums that already have an ISRC or barcode key. It adds links to the
TIDAL tracks or the TIDAL album that carry exactly that value. It never looks up a title or a name,
and it is off without the operator's TIDAL credentials.

| | |
| --- | --- |
| Source | `tidal_isrc`, also-on source |
| Runs on | Another service's track or video with an `isrc:` key, another service's album or YouTube album playlist with a `upc:` key, and MusicBrainz recording, ISRC, release and release-group pages with such a key ([which cards](README.md#lookups-by-isrc-or-barcode)) |
| Asks | TIDAL: `GET https://openapi.tidal.com/v2/tracks?countryCode={country}&filter[isrc]={ISRC}` and `GET …/v2/albums?countryCode={country}&filter[barcodeId]={barcode}` |
| Credentials | Required: the same TIDAL app credentials as `tidal`, `[tidal] client_id` and `client_secret`, or `PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`; off without both |
| Rate | At most 2 requests per second on `openapi.tidal.com`, shared with `tidal`; 1 by default, as `limits.host_requests_per_second` is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | None of the card's own |
| Links | `https://tidal.com/track/{id}` and `https://tidal.com/album/{id}`, exact, keyed by the ISRC or barcode |
| Facts | None |
| Switch | `[also_on.sources.tidal_isrc] enabled`, on by default |

## What it does

TIDAL's own tracks, albums and videos are not asked. The source uses only values that are keys: a
shared ISRC is a fact, and TIDAL would answer with any of its recordings. It asks at most two values
per card. For a track, its own ISRC comes first, then its other `isrc:` keys.

Every answer is read back. Only an item whose own `isrc`, or whose own `barcodeId` as GTIN-13, is
the value asked counts. The catalogue is read in `[tidal] country` (`US` by default), as for
`tidal`.

The source runs after every source that keys an ISRC or a barcode: the service's own record
(`service`, `tidal`, `spotify`), the hubs, `discogs` and `deezer`.

### Tracks

Several TIDAL tracks can carry one ISRC (the recording on several releases). The source takes at
most 5 links in all: first the tracks TIDAL offers for streaming, then TIDAL's order.
`editions` places them, and a card shows one per service. A single ISRC's answer can
have further pages. Only the first is read.

### Albums

Only one TIDAL album can be taken per barcode. The source gives nothing when:

- several TIDAL albums carry the barcode, as with a code such as `0000000000000`;
- the answer has a further page (`links.next`), which may hold another album with the barcode.

## Keys, links and facts

Each link is keyed by the ISRC or barcode that found it. TIDAL's own record of its own item is an
exact source of the value, so a tagged TIDAL link that is one of them joins the group. The source
records no key for the card itself and no facts.

## Cost

One request per ISRC or barcode key, at most two per card. The answers are shared for
`limits.discovery_cache_secs` (an hour by default) by credential pair, and go through the same token
slot as `tidal`'s. Refused credentials (held for `limits.also_on_refusal_hold_secs`, 300 seconds by
default), rate limits and outages are handled as for `tidal`. Each keeps the links an earlier run
found.

To ask for every stored track and album at once, run `pinakesd reextract also-on` with the daemon
stopped. It queues the also-on lookups of every stored resource.

## Turn it off

```toml
[also_on.sources.tidal_isrc]
enabled = false
```

The links it stored stop showing on cards at the next start, and expire as usual. A TIDAL track then
reaches a card only as a link someone tagged, or through MusicBrainz's or Wikidata's relations.
`youtube_match` stops searching TIDAL too, because it does so only while
`tidal_isrc` runs.
