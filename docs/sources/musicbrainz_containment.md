# `musicbrainz_containment`: a recording's releases

This source runs on recording cards that carry one MusicBrainz recording ID. It lists the
official releases that contain that exact recording, as facts. It adds no keys and no links, so it
never joins a recording to an album.

| | |
| --- | --- |
| Source | `musicbrainz_containment`, also-on source |
| Runs on | `MusicRecording` cards with exactly one `mb:recording:…` key (recorded by any source) |
| Asks | MusicBrainz `ws/2`: `GET /release?recording={mbid}&status=official&limit=100` |
| Credentials | None |
| Rate | 1 request per second on `musicbrainz.org`, shared with every other MusicBrainz read |
| Rows live | 30 days; rows expire after 60 |
| Keys | None (facts only) |
| Links | None |
| Facts | `track_of_album` |
| Switch | `[also_on.sources.musicbrainz_containment] enabled`, on by default |

## What it does

1. The source runs only when the card is a `MusicRecording` and its keys name exactly one
   recording. A card with no recording key, or with two, is skipped.
2. It asks MusicBrainz for the official releases of that recording: one page of at most 100
   releases. It doesn't ask for further pages.
3. It records up to eight of those releases as `track_of_album` facts, the eight smallest MBIDs,
   so the same answer always gives the same eight. When there are more, the rest are left out and
   the worker logs the cut.

Releases of every kind count (albums, singles, compilations), as long as MusicBrainz marks them
official.

## Keys, links and facts

| Fact | Meaning |
| --- | --- |
| `track_of_album` | The MusicBrainz page of an official release that contains the recording, `https://musicbrainz.org/release/{mbid}` (at most eight) |

The facts never become keys or [also on](../api.md#also-on) links. The release's key, its barcode
and its pages on music services stay with the release, so the recording joins no album's
[group](../api.md#groups).

## Cost

Each run costs one MusicBrainz request. The answer is cached for `limits.discovery_cache_secs`
(an hour by default), shared with other MusicBrainz reads of the same URL.

- A recording MusicBrainz doesn't know (404) gives no facts, and the source's earlier facts for
  the card are removed.
- An answer without a release list or a release count is retried after a minute.
- Any other failure (a busy MusicBrainz, a 410) is retried later. The card keeps the facts it had
  meanwhile.

## Turn it off

```toml
[also_on.sources.musicbrainz_containment]
enabled = false
```

Its stored `track_of_album` facts stop showing at once. Nothing else on the card changes.
