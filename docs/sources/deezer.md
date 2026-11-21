# `deezer`: Deezer's own ISRCs and barcodes

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`deezer` (on by default; `[also_on.sources.deezer] enabled = false` turns it off): a Deezer
track's or album's own ISRC or barcode from Deezer's API as a key (an ISRC
MusicBrainz gives to several recordings is decided by the track's duration, the one recording
within 3 s of it keyed by its MBID, else the ISRC a fact only: [an ISRC on several
recordings](README.md#an-isrc-on-several-recordings)), and for another service's track or album with an ISRC or barcode key ([which
cards](README.md#lookups-by-isrc-or-barcode)), the one Deezer track or album the API names for
it, keyed by it.

## Operations

`deezer` costs one request per Deezer link and up to two per other track or album with an ISRC or barcode. Deezer's API is paced at 10
requests/s (its published 50 per 5 seconds).

Switched off, no request reaches Deezer's API by any path: the egress refuses `api.deezer.com` while
the source is off. deezer.com links from other sources stay: Wikidata's (P2722, P2723, P2724),
MusicBrainz's, links people tag, and a tagged deezer.com page, which gets its card from the page.
