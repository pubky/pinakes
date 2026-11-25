# `itunes_song`: Apple Music's song for another service's track, by its album and place

This source finds the Apple Music song for a track on another service. It is not an exact lookup:
the song must be at the same place on the same album, with the same title and length. Its link is a
`maybe`, never a key.

| | |
| --- | --- |
| Source | `itunes_song`, also-on source |
| Runs on | Deezer, Spotify and TIDAL tracks, and any other service's track with an `isrc:` key; not Apple Music tracks or videos |
| Asks | iTunes: `GET https://itunes.apple.com/lookup?upc={barcode}&entity=song&country={storefront}`; and, for the track's place, Deezer, Spotify or TIDAL |
| Credentials | None of its own; Spotify and TIDAL places need those services' credentials |
| Rate | About 20 requests per minute on `itunes.apple.com`; the place's service at its own pace |
| Rows live | 30 days; rows expire after 60 |
| Keys | None |
| Links | `https://music.apple.com/song/{trackId}`, a `maybe` |
| Facts | None |
| Switch | `[also_on.sources.itunes_song] enabled`, on by default |

## What it does

The source runs only while `itunes` runs.

1. It reads the track's place: its album's barcode, its disc and track number, its length and its
   title.
2. It asks iTunes for the songs of the album with that barcode, in the storefront
   `[also_on] apple_storefront` (`us` by default).
3. It takes the one song that matches.

### The track's place

The place comes from the track's own service, only while that service's own source runs:

| Service | Read | Where each value is |
| --- | --- | --- |
| Deezer | `track/{id}`, then its album | `disk_number`, `track_position`, `duration`, `title`; the album's `upc` |
| Spotify | `/v1/tracks/{id}`, then its album | `disc_number`, `track_number`, `duration_ms`, `name`; the album's `external_ids.upc` |
| TIDAL | `tracks/{id}?include=albums`, then the album's items | Its one album's `barcodeId`, its `duration`, `title` with `version`; its volume and track number among the album's items. A track on several albums has no one place |

`spotify` and `tidal` run only with the operator's credentials for that service.

For any other track, and when a service refuses the operator's credentials or its source does not
run, the place comes from Deezer's record of the track's ISRC key (`track/isrc:…`), also only while
`deezer` runs. Deezer's track must carry the ISRC asked. The track's own ISRC is asked first, then
the hubs', two at most. A service that answers but gives no place ends the lookup: the ISRC is not
tried.

### The song

The song counts only when:

- iTunes' answer is one collection. iTunes answers several for some codes: the barcode of Underclass
  Hero lists four, two editions of the album and two singles of its title track, so none of its songs
  is taken;
- exactly one song of that collection is at the disc and track number;
- its length (`trackTimeMillis`) is within 3 seconds of the track's. Deezer rounds: its "Get Lucky" is
  367 seconds and Apple's is 369.6;
- its title is the track's own, compared as `same_recording` compares titles. A credit such as
  "(feat. Pharrell Williams and Nile Rodgers)" and a remaster tag go, and a version tag such as
  "Live" stays. A barcode that iTunes answers with one wrong collection (a single's barcode with its
  album) puts no other recording on the card.

## Keys, links and facts

The link is a `maybe` (`kind: "maybe"`, `via` `itunes_song`). iTunes' answers carry no ISRC, so
nothing Apple says ties its song to the track's recording. The album and the place are exact, and
the song at that place is inferred. The link never joins a group and is never a key of the track.
When an exact source finds the same link, its exact link takes the `maybe`'s place. A tagged Apple
Music link stays a separate card unless MusicBrainz or Wikidata relate it.

The source adds no keys and no facts.

## Cost

The place costs a few requests per track:

- a Deezer track: two (the track and its album);
- a Spotify track: two (the track and its album);
- a TIDAL track: up to six (the track with its albums, and up to 5 pages of the album's items);
- a track by its ISRC: up to three (two ISRC lookups and the album). A track whose service refuses the
  credentials costs these as well.

Then comes one iTunes lookup. Each answer is shared for `limits.discovery_cache_secs` (an hour by
default) with the other sources' identical requests. A rate limit or an outage of any of them is
retried, keeping the link an earlier run found. `pinakesd reextract also-on` asks for every stored
track at once.

## Turn it off

```toml
[also_on.sources.itunes_song]
enabled = false
```

Its links stop showing at the next start, and expire as usual.
