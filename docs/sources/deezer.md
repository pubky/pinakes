# `deezer`: Deezer's own ISRCs and barcodes

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`deezer` (off unless the operator turns it on: Deezer's API terms allow non-commercial use
only): a Deezer track's or album's own ISRC or barcode from Deezer's API as a key (an ISRC
MusicBrainz gives to several recordings is decided by the track's duration, the one recording
within 3 s of it keyed by its MBID, else the ISRC a fact only: [an ISRC on several
recordings](README.md#an-isrc-on-several-recordings)), and for another service's track or album with an ISRC or barcode key, the one Deezer
track or album the API names for it, keyed by it.

## Operations

`deezer` is off by default: Deezer's API terms allow non-commercial use only, so turn it on
(`[also_on.sources.deezer] enabled = true`) only where they do; it costs one request per Deezer
link and up to two per other track or album with an ISRC or barcode. Deezer's API is paced at 10
requests/s (its published 50 per 5 seconds).
