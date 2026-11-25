# `deezer`: Deezer's own ISRCs and barcodes

This source reads Deezer's public API, for two kinds of card. A Deezer track or album gets its own
ISRC or barcode. Another service's track or album, when it has an ISRC or barcode key, gets a link to
the Deezer track or album with that value. No key is needed.

| | |
| --- | --- |
| Source | `deezer`, also-on source |
| Runs on | Deezer track and album links; another service's track or video with an `isrc:` key, another service's album or YouTube album playlist with a `upc:` key, and MusicBrainz recording, ISRC and release pages with such a key ([which cards](README.md#lookups-by-isrc-or-barcode)) |
| Asks | `GET https://api.deezer.com/track/{id}` or `/album/{id}`, and `/track/isrc:{ISRC}` or `/album/upc:{barcode}`; MusicBrainz for a Deezer track's ISRC |
| Credentials | None |
| Rate | At most 10 requests per second on `api.deezer.com` (Deezer's published 50 per 5 seconds); 1 by default, as `limits.host_requests_per_second` is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | `upc:…` and `isrc:…` (`mb:recording:…` for an ISRC on several recordings), on Deezer links only |
| Links | `https://www.deezer.com/track/{id}` and `https://www.deezer.com/album/{id}`, exact, keyed by the ISRC or barcode |
| Facts | `isrc` (tracks) and `upc` (albums), on Deezer links only |
| Switch | `[also_on.sources.deezer] enabled`, on by default |

## What it does

### A Deezer track or album

The source reads Deezer's record of the object and takes its ISRC (`isrc`) or barcode (`upc`).

### Another service's track or album

The source asks Deezer for the one track or album it names for the value. It asks at most two values
per card. For a track, its own ISRC comes first, then the hubs' ISRCs. A UPC-A is asked by its 12
digits, without the GTIN-13's leading zero. The answer counts only when its own ISRC or barcode is
the value asked.

Only values that are keys are asked: a shared ISRC is a fact, and Deezer would answer with any of
its recordings. Deezer's own artists and playlists are not read.

## Keys, links and facts

A link is keyed by the ISRC or barcode that found it, so a tagged Deezer link that is it joins the
group.

On a Deezer link, the barcode is always a key. The ISRC of a Deezer track is a key according to what
MusicBrainz knows about it
([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)):

- No recording, or exactly one, has it: the ISRC is the key.
- Several recordings have it and they agree: the keys are the ISRC and the MBID of the recording
  that stands for them.
- Several have it and they disagree: the one recording within 3 seconds of the track's length is
  keyed by its MBID, and the ISRC is a fact. When the length picks none, the ISRC is a fact only.

## Cost

One request per Deezer link, and up to two per other track or album with an ISRC or barcode key. A
Deezer track's ISRC also asks MusicBrainz. The answers are shared for
`limits.discovery_cache_secs` (an hour by default).

Deezer answers some errors with status 200:

| Deezer's answer | Result |
| --- | --- |
| 404, or error 800 ("no data"), 500, 501 or 600 | Deezer has none |
| Error 4 (quota) or 700 ("service busy"), 429, or a 5xx | Retried after the wait Deezer named, held between a minute and an hour (`deezer_busy`) |
| Any other status or error code | The run fails and is retried; the rows an earlier run stored stay |

Other sources also ask Deezer's API, only while this source is on: `itunes_song`,
`youtube_match` and `editions`.

## Turn it off

```toml
[also_on.sources.deezer]
enabled = false
```

No request reaches `api.deezer.com` by any path: the egress refuses that host while the source is
off (and while `[also_on] enabled = false`). The rows it stored stop showing at the next start, and
expire as usual.

Links to deezer.com from other sources stay: Wikidata's (P2722, P2723, P2724), MusicBrainz's, and
links people tag. A tagged deezer.com page gets its card from the page.
