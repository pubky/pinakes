# `musicbrainz`: what MusicBrainz relates a music link to

This source runs on links to tracks, albums and artists on music services. It asks MusicBrainz
which entity the exact link is related to, then records that entity's IDs as keys and its other
music-service pages as [also on](../api.md#also-on) links. It never matches by title or name.

| | |
| --- | --- |
| Source | `musicbrainz`, also-on source |
| Runs on | Track, video, album, YouTube album playlist and artist links on Spotify, Apple Music, YouTube, Deezer, TIDAL, Qobuz, SoundCloud, Amazon Music, Bandcamp and Discogs |
| Asks | MusicBrainz `ws/2`: the URL lookup (`GET /url?resource={link}`), the entity read (`GET /{kind}/{mbid}`), and as needed the ISRC lookup, a barcode search and the recording's official albums |
| Credentials | None |
| Rate | 1 request per second on `musicbrainz.org` (MusicBrainz's published limit), shared with every other MusicBrainz read |
| Rows live | 30 days; rows expire after 60 |
| Keys | `mb:recording:…`, `mb:release:…`, `mb:release-group:…`, `mb:artist:…`, `isrc:…`, `upc:…` |
| Links | The entity's current pages on music services at the link's level, exact (keyed by the entity) |
| Facts | `mbid`, `mbid_was`, `isrc`, `upc`, `artist`, `duration`, `artist_type`, `original_album` |
| Switch | `[also_on.sources.musicbrainz] enabled`, on by default |

## What it does

The source works at the link's level. A track link (and a video, which counts as its recording)
is looked up as a recording. An album link (and a YouTube playlist, which counts as the album it
holds) is looked up as a release or release group. An artist link is looked up as an artist.
Playlists on other services are not read.

1. It asks MusicBrainz for the entities the link is related to (`GET /url?resource={link}`). Only
   current relations count; ended ones are ignored. MusicBrainz stores URLs as editors entered
   them, so the link is sent as the card has it. An Apple Music link is asked under 20
   storefronts' spellings (`us`, `gb`, `de`, …) in one request, and takes every spelling's
   answer.
2. It picks the entities at the link's level:
   - A track takes the recordings, an artist link the artists.
   - An album takes the releases. When the link is related to no release, it takes the release
     groups instead.
   - A Discogs master is a release group and takes only release groups. A Discogs release is one
     edition and takes only releases.
3. Exactly one entity is a match. Several are ambiguous and give nothing, with one exception: up
   to four releases of one release group (editions MusicBrainz keeps apart) count as that release
   group. Releases of different release groups give nothing.
4. It reads each matched entity (`GET /{kind}/{mbid}`) with its URL relations, ISRCs, barcode and
   artist credit, and records what the entity says (see the next section).

### A track MusicBrainz doesn't relate by URL

When no recording is related to a track link, the source can start from the ISRC the track's own
page states, but only for a service that vouches for it (TIDAL). It looks the ISRC up
(`GET /isrc/{ISRC}`):

- One recording has the ISRC: that recording is the match.
- Several recordings have it and they agree (one recording MusicBrainz hasn't merged yet): the
  source takes them as one recording. See
  [an ISRC on several recordings](README.md#an-isrc-on-several-recordings).
- Several recordings have it and they disagree: the track's length on its service picks one when
  exactly one recording is within 3 seconds of it and every other recording's length is known and
  is not. The ISRC then stays a fact, because it names the other recordings too.
- Otherwise nothing matches.

### An album MusicBrainz doesn't relate by URL

When no release or release group is related to an album link, the source can start from the
album's barcode. It uses only `upc:` keys that another source recorded, at most two: TIDAL's or
Spotify's own record of the album in the same run, or a later source's (such as Deezer's or
Discogs') from an earlier run. It never uses one a MusicBrainz source recorded. Otherwise a
release whose URL relation an editor removed would be found again by its own barcode.

For each barcode it searches MusicBrainz's barcode field alone (`GET /release?query=barcode:…`,
asking for a 12-digit UPC in both spellings), and keeps only releases whose barcode is exactly
that one. The releases found then go through steps 3 and 4 above, as if the URL named them. A
Discogs master is never looked up by barcode, because barcodes name releases.

### A Discogs master

A release group's entry on MusicBrainz names no service pages (its editions have them). For a
Discogs master, the source also reads the Wikidata item the release group's entry is related to,
when there is exactly one such item and that item names this release group back (`P436`). The
album pages that item names (see `wikidata` for the properties) are added as
links, keyed by the release group.

## Keys, links and facts

### Keys

| Matched | Keys |
| --- | --- |
| One recording | `mb:recording:{mbid}`, and each of its ISRCs as `isrc:…` (at most eight) when that ISRC names this recording alone |
| One release | `mb:release:{mbid}`, `mb:release-group:{mbid}` (not for a Discogs release, which is an edition), and its barcode as `upc:…` |
| Several releases of one release group | `mb:release-group:{mbid}` alone, so the editions stay apart, and `upc:…` when every release has the same barcode |
| One release group | `mb:release-group:{mbid}` |
| One artist | `mb:artist:{mbid}` |

A recording that is one of several agreeing duplicates is keyed by the one that stands for them
all (the smallest MBID); its own MBID becomes the fact `mbid_was`. An ISRC is a key only when
MusicBrainz names this recording alone for it, or knows no recording with it. An ISRC shared with
a recording that disagrees stays a fact: as a key it would join whichever recording's group took
it first. ISRCs are stored in upper case, and barcodes as GTIN-13 (a 12-digit UPC gains a leading
zero).

With these keys the link joins the [group](../api.md#groups) of every other link and card that
carries the same key.

### Links

The entity's current URL relations that are music-service pages at the link's level become
derived links: a track lists tracks and videos of the recording, an album lists albums and YouTube
album playlists, an artist lists artist pages. A Discogs page counts only at its own level (an
artist's artist page, a release group's master, a release's release). For several releases of one
release group, only the links every release has are kept.

Every link is exact and keyed by the entity that names it (the release group for several
releases). A tagged link that is one of them takes the key and joins the group, even when its own
URL lookup is ambiguous.

### Facts

| Fact | Meaning |
| --- | --- |
| `mbid` | The MusicBrainz ID of the matched entity (the standing recording for duplicates) |
| `mbid_was` | The MBID of a duplicate recording keyed by another one |
| `isrc` | Each ISRC the recording carries, keyed or not |
| `upc` | The release's barcode, as GTIN-13 |
| `artist` | The artist credit, as MusicBrainz writes it |
| `duration` | The recording's length in seconds |
| `artist_type` | An artist's type (`Person`, `Group`, …) |
| `original_album` | A recording's original album: the title of the release group of its earliest official release that is an album with no secondary type (no compilation, live or remix album) |

For several releases, only the facts every release has are kept.

## Cost

Each link costs one URL lookup and one read per matched entity (at most four for releases). A
recording costs one more request for its original album. Depending on the path, more requests
follow:

- a track found by its ISRC: one ISRC lookup;
- an album found by its barcode: up to two barcode searches;
- a Discogs master: one Wikidata read;
- a recording's ISRCs: one ISRC lookup each, to see whether the ISRC names this recording alone.

Checking for duplicate recordings reads at most 8 recordings, 16 ISRC lookups and 32 recording
records per check. Every MusicBrainz and Wikidata answer is cached for
`limits.discovery_cache_secs` (an hour by default), so sources that ask the same thing ask once.

The worker asks MusicBrainz about a batch's links together, up to 50 URLs per request
([operations](../operations.md#also-on)). A link a batch answered asks nothing more for its URL
lookup.

An answer larger than `limits.hub_body_bytes` (8 MiB by default) gives nothing for that link. A
busy MusicBrainz (a 5xx answer) is retried later, and the link keeps its earlier rows meanwhile.

To use a MusicBrainz mirror, set its web service in `[also_on.sources.musicbrainz] base_url` (an
`http(s)` URL without a trailing slash). Every MusicBrainz read, including MusicBrainz pages'
cards, then goes there.

## Turn it off

```toml
[also_on.sources.musicbrainz]
enabled = false
```

Its stored keys, links and facts stop showing at once and its groups re-form without them. They
show again if you turn it back on before they expire.
