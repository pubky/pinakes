# `tidal_isrc`: another service's track or album on TIDAL, by its ISRC or barcode

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`tidal_isrc` (**off by default**; on only with `[also_on.sources.tidal_isrc] enabled = true` and
the operator's TIDAL credentials, `[tidal] client_id` and `client_secret` or
`PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`, the same as `tidal`'s): for a track
on another service with an `isrc:` key (Deezer's or Spotify's own record, a vouching page's, a
hub's), the TIDAL tracks that carry exactly that ISRC, from TIDAL's API
(`GET /v2/tracks?countryCode={[tidal] country}&filter[isrc]=…`); for an album on another service
with a `upc:` key, the TIDAL album that carries exactly that barcode
(`GET /v2/albums?countryCode=…&filter[barcodeId]=…`). Each answer is read back: only an item whose
own `isrc` or `barcodeId` (as GTIN-13) is the value asked counts. The links are
`https://tidal.com/track/{id}` and `https://tidal.com/album/{id}`, keyed by the ISRC or barcode:
TIDAL's own record of its own item is an exact source of it, so a tagged TIDAL link that is one of
them joins the group.

The track's own ISRC is asked first, then the hubs', two at most; only an ISRC that is a key (a
shared one is a fact, and TIDAL would answer with any of its recordings). One ISRC may name several
TIDAL tracks (the recording on several releases): at most 5, those TIDAL offers for streaming
first, then in TIDAL's order; [`editions`](editions.md) places them and a card shows one per
service. A barcode several TIDAL albums carry names none of them and gives nothing (TIDAL answers
unrelated albums for a code such as `0000000000000`). No keys of the resource and no facts. TIDAL's
own tracks and albums are not asked. It runs after every source that keys an ISRC or a barcode
(the service's own record, `tidal`, `spotify`, the hubs, `discogs`, `deezer`).

## Why it is off by default

TIDAL's Developer Guidelines (version 3.0, 6 May 2024) list "Integrating TIDAL content with
content or streams from another service" among the offerings that are "strictly prohibited unless
you have TIDAL's express written approval", and an "Also on" list puts a TIDAL link beside other
services' links. Its Design Guidelines (version 3.0) say not to "display TIDAL content next to
content from similar services". Its Developer Terms (version 3.0) also allow TIDAL content only
"the temporary caching of metadata and cover art" and forbid stored "compilations or databases of
TIDAL Content"; a derived link lives the source's lifetime (30 days, twice that in the table) like
every derived link. Turning this source on is the operator's decision on those terms (TIDAL's
written approval, or their own reading); without it, a TIDAL track reaches a card as a link someone
tagged, or through MusicBrainz's or Wikidata's relations.

## Operations

`tidal_isrc` costs one request per ISRC or barcode key of a track or album on another service (two
at most), shared for an hour by credential pair, on the same pace as `tidal` (2 requests/s on
`openapi.tidal.com`), through the same token slot: refused credentials (the token endpoint refuses
them, or the API answers 403) hold five minutes (`tidal_auth`), a rate limit or an outage is waited
out for the time TIDAL asked (`tidal_busy`), and either keeps the links an earlier run found. A
single ISRC's answer is paged; only its first page is read. Switched off, the rows it stored stop
showing on cards and expire as usual; switched on after an upgrade, `pinakesd reextract also-on`
asks for every stored track and album at once. The credentials and the token never appear in logs,
the operator info, errors or debug output.
