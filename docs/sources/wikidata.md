# `wikidata`: the Wikidata item of a music link

This source runs on links to tracks, albums and artists on music services. It finds the one
Wikidata item that carries the link's own service ID, then records the item's other
music-service pages as [also on](../api.md#also-on) links and its Q-id and MusicBrainz ID as
keys. It never matches by title or name.

| | |
| --- | --- |
| Source | `wikidata`, also-on source |
| Runs on | Music links whose service and level have a Wikidata property (table below) |
| Asks | Wikidata: a search by the service ID (`GET https://www.wikidata.org/w/api.php?action=query&list=search&srsearch=haswbstatement:{property}={id}`), then the items found (`action=wbgetentities`); MusicBrainz for a recording item's MBID |
| Credentials | None |
| Rate | 200 requests a minute on `www.wikidata.org`; 1 request per second on `musicbrainz.org` |
| Rows live | 30 days; rows expire after 60 |
| Keys | `wikidata:Q…`, `mb:recording:…`, `mb:release-group:…`, `mb:artist:…` |
| Links | The item's pages on music services at the link's level, exact (keyed by the Q-id) |
| Facts | `mbid_was` |
| Switch | `[also_on.sources.wikidata] enabled`, on by default |

## What it does

### The links it reads

The source runs on a link when Wikidata has a property for the link's service at the link's
level:

| Service | Track | Album | Artist |
| --- | --- | --- | --- |
| Spotify | `P2207` | `P2205` | `P1902` |
| Apple Music | `P10110` | `P2281` | `P2850` |
| Deezer | `P2724` | `P2723` | `P2722` |
| TIDAL | `P4578` | `P4577` | `P4576` |
| Qobuz | | `P11770` | `P11578` |
| YouTube | `P1651` (a video) | `P4300` (a playlist) | `P2397` (a channel) |
| SoundCloud | | | `P3040` |
| Amazon Music | `P13398` | | `P6276` |
| Bandcamp | | | `P3283` |
| Discogs | | `P1954` (a master); `P2206` (a release) | `P1953` |

A Discogs ID is asked as the number alone. The same table turns an item's IDs back into links.
Only `P2206` isn't in it: an album item lists its Discogs master, and a Discogs release names an
edition.

### Finding the item

1. The source searches Wikidata for items with the statement `{property}={id}` (at most five
   hits). When Wikidata reports more than five, nothing matches.
2. It reads every hit back and keeps the items that carry the ID at their best rank (preferred
   if any, else normal; never deprecated). The search index only proposes, so a stale index or
   a deprecated claim never matches.
3. Of several items that carry the ID, one that another of them replaces gives way to its
   successor. Two or more items left are ambiguous and give nothing.

### The item must be of the link's level

A key never crosses levels, so the item must be what the link is:

- A track takes only a recording's item: one MusicBrainz recording ID (`P4404`), no work,
  release, release group or artist ID, and not a song or single by class. A song's item is a
  work that its studio, live and remastered recordings share, so a track takes neither its Q-id
  nor its links.
- An album takes only an album's item: a MusicBrainz release group ID (`P436`) or a Discogs
  master ID (`P1954`), and no artist ID. An item that is also its single's song's counts. Such an
  item is never a track's, so no Q-id keys both a track and an album.
- A Discogs release takes only an edition's item: a MusicBrainz release ID (`P5813`), or no
  MusicBrainz ID yet. An item that is an album's, a recording's, a work's or an artist's gives
  nothing.
- An artist link takes the item that carries its ID.

## Keys, links and facts

A matched item gives:

- the key `wikidata:{Q-id}`;
- its MusicBrainz ID at the link's level as a key, when the item has exactly one: `mb:recording:…`
  (`P4404`) for a track, `mb:release-group:…` (`P436`) for an album, `mb:artist:…` (`P434`) for
  an artist. Several IDs mean the item spans entities, and none becomes a key;
- its pages on music services at the link's level (up to four per property), each keyed by the
  Q-id, so a tagged link that is one of them joins the [group](../api.md#groups).

A recording's MBID is keyed as every source keys a recording: when it is one of several
duplicate recordings, by the one that stands for them all
([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)). The item's own MBID
then stays as the fact `mbid_was`, so `key=mbid:{mbid}` still finds the track by it.

| Fact | Meaning |
| --- | --- |
| `mbid_was` | The item's recording MBID, when another recording stands for it |

## Cost

Each link costs two Wikidata requests: the search and one read of its hits together. The search
by a service ID can't be batched. When the hits together are larger than `limits.hub_body_bytes`
(8 MiB by default), each is read alone. A recording item costs MusicBrainz reads to settle its
duplicates. Every answer is cached for `limits.discovery_cache_secs` (an hour by default).

An answer larger than the cap gives nothing for that link. A busy Wikidata is retried later, and
the link keeps its earlier rows meanwhile.

## Turn it off

```toml
[also_on.sources.wikidata]
enabled = false
```

Its stored keys and links stop showing at once and its groups re-form without them.
