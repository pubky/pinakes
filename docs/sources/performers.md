# `performers`: an event's performers, keyed to the artist on the page's own service

This source runs on event pages that name their performers in their structured data. It records
each performer's name, and the performer's MusicBrainz artist as a key only when the event page
belongs to the service of the artist link it gives. A name alone never becomes a key, and nothing
is searched by name.

| | |
| --- | --- |
| Source | `performers`, also-on source |
| Runs on | Event cards (`MusicEvent` or any other `Event`) whose page names performers (schema.org `performer`) |
| Asks | MusicBrainz `ws/2`: one URL lookup for all the page's performer links (`GET /url?resource=…&resource=…`) |
| Credentials | None |
| Rate | 1 request per second on `musicbrainz.org`, shared with every other MusicBrainz read |
| Rows live | 30 days; rows expire after 60 |
| Keys | `mb:artist:…` |
| Links | None |
| Facts | `performer`, `performer_mbid`, `performer_link` |
| Switch | `[also_on.sources.performers] enabled`, on by default |

## What it does

### Which performers it reads

The page's card holds the performers its own event names in its structured data: the `performer`
values of that event, each with a name and the links the page gives for it (`url`, `sameAs`).
Another event the page describes beside its own (a venue's next show) gives none. A card keeps up
to 16 performers with up to 8 links each, within 2 KB.

For each performer (at most 16), the source keeps at most two links that MusicBrainz can relate
to an artist:

- an artist page on a music service (Spotify, Apple Music, Deezer, TIDAL, …);
- a Songkick artist page (`songkick.com/artists/{number}-…`), asked as
  `https://www.songkick.com/artists/{number}`, the form MusicBrainz stores;
- a Bandsintown artist page (`bandsintown.com/a/{number}-…`), asked as
  `https://www.bandsintown.com/a/{number}`.

A MusicBrainz artist page the page names for the performer is kept separately, as the page's own
claim.

### Key or fact

The source asks MusicBrainz about every kept link of every performer in one request. A link
MusicBrainz relates to exactly one artist names that artist. Then, for each performer:

1. When the event page is on the same service (host) as a link that names an artist, that
   artist's MBID becomes the key `mb:artist:{mbid}` and the fact `performer_mbid`. For example, a
   Songkick concert page naming a Songkick artist page, a Bandsintown event naming a Bandsintown
   artist page, or a music service's own event page naming that service's artist page.
2. Otherwise, the artist MusicBrainz relates another link to, else the MusicBrainz artist page
   the page names, is the fact `performer_mbid` only. Any page can name any artist, so this
   identity is the page's choice, not the service's.
3. A performer with neither keeps the first of its kept links as the fact `performer_link`.

The event page's service is the host its fetch ended on, after redirects: a Songkick address that
redirects off Songkick counts as the other site's page. A stored card that doesn't record where
its performers were read gives no key until its page is read again.

## Keys, links and facts

| Fact | Meaning |
| --- | --- |
| `performer` | A performer's name, as the page gives it |
| `performer_mbid` | The performer's MusicBrainz artist ID, keyed or not |
| `performer_link` | The first of the performer's kept links, when no artist is known |

Events [group](../api.md#groups) only with events, so the `mb:artist:…` key never joins a concert
to the artist's own pages. It makes the concert one of the artist's events. To list an artist's
upcoming concerts, soonest first:

```
GET /v1/search?key=mbid:{artist}&type=MusicEvent&from=now&sort=start
```

A performer the source doesn't key is in the card's facts only and isn't found by `key=`. Text
search (`q=`) matches an event's name and description, not its performers: an act named only in
`performer` (a festival's support act) isn't found by it.

## Cost

Each event page costs at most one MusicBrainz request, for all its performers' links together. A
performer named only by a MusicBrainz artist page costs nothing. Answers are cached for
`limits.discovery_cache_secs` (an hour by default), so an artist's many concerts that name the
artist by one link ask once.

A link the answer leaves out is unknown until the next run. When the request fails (a busy
MusicBrainz, or an answer larger than `limits.hub_body_bytes`):

- A page on the service of one of its performer links fails its run. It keeps its earlier rows
  and retries later.
- Any other page records what it says without MusicBrainz (names, claimed MBIDs, links), and asks MusicBrainz again
  after MusicBrainz's retry time.

`[also_on.sources.musicbrainz] base_url` points these reads at a MusicBrainz mirror, as for the
other MusicBrainz sources.

## Turn it off

```toml
[also_on.sources.performers]
enabled = false
```

Its stored keys and facts stop showing at once, so `key=mbid:{artist}&type=MusicEvent` doesn't
list the events it keyed. The events' own cards, with their performers, stay as they are.
