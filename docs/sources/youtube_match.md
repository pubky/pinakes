# `youtube_match`: a label's YouTube upload on Deezer and TIDAL, by artist, title and length

This source finds a label's YouTube upload on Deezer and TIDAL. It is not an exact lookup: it compares
the artist, the title and the length. Its links are `maybe` links, never keys.

| | |
| --- | --- |
| Source | `youtube_match`, also-on source |
| Runs on | YouTube and YouTube Music videos that are a label's upload, as their stored card shows |
| Asks | YouTube's watch page `https://www.youtube.com/watch?v={id}`; Deezer `GET https://api.deezer.com/search?q={artist} {title}` while `deezer` runs; TIDAL `GET https://openapi.tidal.com/v2/searchResults?countryCode={country}&filter[query]={artist} {title}&include=tracks.artists` while `tidal_isrc` runs |
| Credentials | None of its own; the TIDAL search needs the operator's TIDAL credentials |
| Rate | `www.youtube.com` at the shared per-host default (`limits.host_requests_per_second`), and Deezer's and TIDAL's paces |
| Rows live | 30 days; rows expire after 60 |
| Keys | None |
| Links | `https://www.deezer.com/track/{id}` and `https://tidal.com/track/{id}`, a `maybe` each |
| Facts | `duration`, in seconds |
| Switch | `[also_on.sources.youtube_match] enabled`, on by default |

## What it does

### Which videos

A video is a label's upload when its stored card shows one of these:

- its channel (the oEmbed `author`) is an artist's Topic channel, `<Artist> - Topic`;
- its description starts "Provided to YouTube by".

The source runs only while `deezer` or `tidal_isrc` runs.

### What it reads

The stored card has no length, so the source reads the watch page: the title, channel, length and
description in `ytInitialPlayerResponse.videoDetails` (`title`, `author`, `lengthSeconds`,
`shortDescription`). When the length is missing there, it takes the page's
`<meta itemprop="duration">`. The artist is the Topic channel's name, else the first artist of the
description's "Title · Artist" line.

The length becomes the card's `duration` fact, whatever the searches find.

### What counts as a match

The source searches each service for the artist and title as `same_recording` compares them: lower
case, without the tags that keep a recording's identity (such as a remaster), and with the first
credited artist only. Only the first page of each search is read, so a second such track further
down goes unseen.

For each service, exactly one track must have all of these:

- the same artist (the track's first);
- the same title, with its version, such as `Bohemian Rhapsody (Live Aid)`;
- a length within 2 seconds of the video's.

None or several give nothing for that service. Queen's Topic upload of "Bohemian Rhapsody" (354
seconds) matches Deezer's one such track. TIDAL has two (355 and 356 seconds), so TIDAL gives none.

### From other services to YouTube

This source goes from a YouTube video to other services only. The exact links from another service's
track to its YouTube video come from MusicBrainz URL relations (`musicbrainz`) and
Wikidata's YouTube video ID, P1651 (`wikidata`).

## Keys, links and facts

The links are `maybe` links (`kind: "maybe"`, `via` `youtube_match`). Nothing YouTube publishes ties a
video to an ISRC or another service's ID, so the artist, title and length are a metadata match. The
links never join a group and never become a key of the video. When an exact source finds the same
link, its exact link takes the `maybe`'s place. A candidate's ISRC is never recorded as the video's.

## Cost

For each label upload: one watch page (about 1.4 MB, refused above
`limits.also_on_watch_page_bytes`, 3 MiB by default), then one Deezer search and one TIDAL search.
Each answer is shared for `limits.discovery_cache_secs` (an hour by default).

### Failures

| What happens | What the source does |
| --- | --- |
| YouTube serves a sign-in page without video details (`playabilityStatus.status` `LOGIN_REQUIRED`), another wall, or a 403 | Retries six hours later (`youtube_login`), keeping what an earlier run found |
| YouTube answers 429 or a 5xx | Retries after the wait YouTube named (`youtube_busy`), keeping what an earlier run found |
| The watch page is over the cap, or has no video details | Finds nothing, drops what an earlier run found, and does not retry |
| Deezer's search fails (busy, down, or an unexpected answer) | Retries, keeping what an earlier run found |
| TIDAL fails in a run with a Deezer match | Records the Deezer match beside the links an earlier run found. The run comes back after the wait TIDAL named, else after `limits.also_on_refusal_hold_secs` (300 seconds by default) |
| TIDAL fails in a run without a Deezer match | Fails the run, which keeps every link an earlier run found |
| TIDAL refuses the credentials | Records a Deezer match if there is one, and keeps every link an earlier run found. No run comes back early: the video is read again at its usual time |

`pinakesd reextract also-on` asks for every stored video at once.

## Turn it off

```toml
[also_on.sources.youtube_match]
enabled = false
```

Its links and the `duration` fact stop showing at the next start, and expire as usual.
