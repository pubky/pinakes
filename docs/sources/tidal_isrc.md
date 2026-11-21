# `tidal_isrc`: another service's track or album on TIDAL, by its ISRC or barcode

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`tidal_isrc` (on by default; it runs only with the operator's TIDAL credentials, `[tidal]
client_id` and `client_secret` or `PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`, the
same as `tidal`'s; `[also_on.sources.tidal_isrc] enabled = false` turns it off; [which
cards](README.md#lookups-by-isrc-or-barcode)): for a track (or a
video of a recording) on another service with an `isrc:` key (Deezer's or Spotify's own record, a
vouching page's, a hub's), the TIDAL tracks that carry exactly that ISRC, from TIDAL's API
(`GET /v2/tracks?countryCode={[tidal] country}&filter[isrc]=…`); for an album on another service
with a `upc:` key, the TIDAL album that carries exactly that barcode
(`GET /v2/albums?countryCode=…&filter[barcodeId]=…`). Each answer is read back: only an item whose
own `isrc` or `barcodeId` (as GTIN-13) is the value asked counts. The links are
`https://tidal.com/track/{id}` and `https://tidal.com/album/{id}`, keyed by the ISRC or barcode:
TIDAL's own record of its own item is an exact source of it, so a tagged TIDAL link that is one of
them joins the group.

The track's own ISRC is asked first, then the hubs', two at most; only an ISRC that is a key (a
shared one is a fact, and TIDAL would answer with any of its recordings). One ISRC may name several
TIDAL tracks (the recording on several releases): at most 5 links in all, those TIDAL offers for
streaming first, then in TIDAL's order; [`editions`](editions.md) places them and a card shows
one per service. A barcode several TIDAL albums carry names none of them and gives nothing (TIDAL
answers unrelated albums for a code such as `0000000000000`), and so does an album answer with a
further page (`links.next`), which may hold another album with the barcode. No keys of the
resource and no facts. TIDAL's own tracks and albums are not asked. It runs after every source
that keys an ISRC or a barcode (the service's own record, `tidal`, `spotify`, the hubs, `discogs`,
`deezer`).

## Operations

`tidal_isrc` costs one request per ISRC or barcode key of a track or album on another service (two
at most), shared for an hour by credential pair, on the same pace as `tidal` (2 requests/s on
`openapi.tidal.com`), through the same token slot: refused credentials (the token endpoint refuses
them, or the API answers 403) hold five minutes (`tidal_auth`), a rate limit or an outage is waited
out for the time TIDAL asked (`tidal_busy`), and either keeps the links an earlier run found. A
single ISRC's answer is paged; only its first page is read. Switched off, the rows it stored stop
showing on cards and expire as usual; after an upgrade that adds it, `pinakesd reextract also-on`
asks for every stored track and album at once. The credentials and the token never appear in logs,
the operator info, errors or debug output.
