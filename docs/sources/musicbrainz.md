# `musicbrainz`: what MusicBrainz relates a music link to

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`musicbrainz`: what MusicBrainz relates the link to (one recording, release or release group,
or artist; several are ambiguous and give nothing), that entity's current links on music
services, and its MBIDs, the recording's ISRCs (up to eight; one MusicBrainz also gives to a
recording that disagrees is a fact, not a key) and the release's barcode as keys
(`mb:recording:…`, `mb:release:…`, `mb:release-group:…`, `mb:artist:…`, `isrc:…`,
`upc:…`). A track MusicBrainz does not relate by URL takes the recording its page's own ISRC
is on when the page's service vouches for the ISRC (TIDAL's) and
MusicBrainz knows exactly one recording with it, or the one of several the page's duration picks
(within 3 s of its length, no other: [an ISRC on several
recordings](README.md#an-isrc-on-several-recordings)). An
album it does not relate by URL takes the releases carrying the album's barcode keys (TIDAL's
own record, or a service's or Discogs' from an earlier run; at most two, never one a
MusicBrainz source found), by a search on the barcode field alone (a 12-digit UPC in both
spellings) checked against the answer: one release, or editions of one release group
(described next). An album link related to up to four releases of one release group (editions
MusicBrainz keeps apart) takes the release group's key alone, and the links, facts and barcode
every one of those releases has; releases of different release groups are ambiguous. Its facts: `mbid`, the recording's
`isrc`s, the release's barcode as `upc`, the `artist` credit, the recording's `duration` and
`original_album` (the earliest official album that is no compilation, live or remix album),
and an artist's `artist_type` (`Person`, `Group`, …). ISRCs are stored in upper case and barcodes as
GTIN-13 (a 12-digit UPC gains a leading zero), by every source.
Discogs pages are taken at their level only: a Discogs master is a release group (never
looked up by barcode), a Discogs release one release and never its release group (an edition is not the album), a Discogs
artist an artist. Each lists its entity's service pages; a release group's MusicBrainz entry
names none (its editions have them), so a master lists the album pages of the one Wikidata
item MusicBrainz relates the release group to, when that item names the release group back
(P436), keyed by the release group.

## Operations

Each music link costs one MusicBrainz request for its entity (up to four when the URL names
several releases; one more for a track found by its ISRC, up to two barcode searches for an album
found by its barcode keys (only those another source found) and up to four release reads for what
they name, and one for a track's original album). The duplicate check (an ISRC MusicBrainz
gives to several recordings, [an ISRC on several recordings](README.md#an-isrc-on-several-recordings))
costs, for a recording a link is related to, one ISRC lookup per ISRC of the recording and,
where several recordings share one, each of their records (at most 8 recordings, 16 ISRC
lookups and 32 recording reads per walk), all cached; a duplicate's other ISRCs are each looked up
once more to see whether they name the same recording. The worker asks MusicBrainz about a batch's
links' URLs together ([operations](../operations.md#also-on)); an Apple Music link, which
MusicBrainz stores under the storefront an editor browsed, is asked under twenty storefronts'
spellings in one request.
