# `youtube_match`: a label's YouTube upload on Deezer and TIDAL, by artist, title and length

One of the music sources of [also on](../api.md#also-on). Unlike the exact lookups, its links are
matches by artist, title and length: maybes (`kind` `maybe`), never keys.

`youtube_match` (on by default; `[also_on.sources.youtube_match] enabled = false` turns it off):
for a YouTube or YouTube Music video that is a label's upload, the same recording on Deezer and on
TIDAL. A video is a label's upload when its stored card shows one: its channel (the oEmbed
`author`) is an artist's Topic channel, `<Artist> - Topic`, or its description starts "Provided to
YouTube by". The source reads the video's watch page (`https://www.youtube.com/watch?v={id}`) for
its title, channel, length and description (`ytInitialPlayerResponse.videoDetails`:
`title`, `author`, `lengthSeconds`, `shortDescription`; the page's `<meta itemprop="duration">`
when the length is missing), as the stored card has no length. The artist is the Topic channel's
name, else the first artist of the description's "Title · Artist" line.

It then searches, for the artist and title as `same_recording` compares them (lower case, the tags
that keep a recording's identity such as a remaster dropped, the first credited artist):
- Deezer (`https://api.deezer.com/search?q={artist} {title}`, the plain form, which finds
  tracks the advanced `artist:"…" track:"…"` form misses), while `deezer` runs;
- TIDAL (`GET /v2/searchResults?countryCode={[tidal] country}&filter[query]={artist} {title}&include=tracks.artists`,
  with the operator's TIDAL credentials), while `tidal_isrc` runs.

Of each service's tracks, exactly one must have the same artist (its first) and title (with its
version, `Bohemian Rhapsody (Live Aid)`) and a length within 2 s of the video's; none or several
give nothing for that service. Only the first page of each search is read (25 Deezer tracks, 20
TIDAL tracks), so a second such track further down goes unseen; the link is a maybe, never a
join. Queen's Topic upload of Bohemian Rhapsody (354 s) matches Deezer's one such track (354 s);
TIDAL has two (355 s and 356 s), so it gives no TIDAL link. The links are
`https://www.deezer.com/track/{id}` and `https://tidal.com/track/{id}`, `via` `youtube_match`,
`kind` `maybe`.

## Why it is a maybe

Nothing YouTube publishes ties a video to an ISRC or another service's ID: the artist, title and
length are a metadata match. Its links are derived but unkeyed and marked `maybe`: they never
join a group, never become a key of the video, and a candidate's ISRC is never recorded as the
video's.

YouTube sometimes answers a watch page with a sign-in request
(`playabilityStatus.status` `LOGIN_REQUIRED`, "confirm you're not a bot") and no video details,
which the egress takes for a wall (as any other wall YouTube puts up), or a 403. That is no answer:
the lookup is retried six hours later (`youtube_login`), keeping what an earlier run found.

## From other services to YouTube

The exact paths from another service's track to its YouTube video are MusicBrainz URL relations
(`musicbrainz`) and Wikidata's YouTube video ID (P1651, `wikidata`), which already give YouTube
links. Odesli (song.link) is not used: its public API answers 401 `PUBLIC_API_ACCESS_DEPRECATED`
(version v1-alpha.1 retired on 31 July 2026, no new keys), and it matches by metadata too. No
other exact source of a track's YouTube video is known.

## Operations

Per label upload, while Deezer or `tidal_isrc` runs: one watch page (about 1.4 MB, refused above
3 MB, shared for an hour) at the host's default pace (`limits.host_requests_per_second`, one
request at a time), then one Deezer search and one TIDAL search, each shared for an hour. A
rate limit or an outage of YouTube or Deezer (`youtube_busy`, Deezer's quota) is retried, keeping
the links an earlier run found. TIDAL failing (refused credentials, a rate limit, an unexpected
answer) fails the run, which keeps every link an earlier run found, unless the run has a Deezer
match: that is recorded without a TIDAL link (an earlier TIDAL link is out until the retry), and
the run comes back after the wait TIDAL named, else after five minutes. After an upgrade that adds
it, `pinakesd reextract also-on` asks for every stored video at once.
