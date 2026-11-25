# `musicbrainz_entity`: a MusicBrainz page as its own hub

This source runs on MusicBrainz pages. It reads the entity the page names and records its IDs as
keys and its music-service pages as [also on](../api.md#also-on) links, the same way
`musicbrainz` does for a service link MusicBrainz relates to an entity.

| | |
| --- | --- |
| Source | `musicbrainz_entity`, also-on source |
| Runs on | Recording, release, release group and artist pages, and ISRC pages, on `musicbrainz.org` (all their [spellings](../api.md#aliases)) |
| Asks | MusicBrainz `ws/2`: the entity read (`GET /{kind}/{mbid}`) or the ISRC lookup (`GET /isrc/{ISRC}`); Wikidata for an artist's or release group's own item |
| Credentials | None |
| Rate | 1 request per second on `musicbrainz.org`; 200 requests a minute on `www.wikidata.org` |
| Rows live | 30 days; rows expire after 60 |
| Keys | `mb:recording:…`, `mb:release:…`, `mb:release-group:…`, `mb:artist:…`, `isrc:…`, `upc:…`, `wikidata:Q…` |
| Links | The entity's current pages on music services at its level, exact (keyed by the entity) |
| Facts | `mbid`, `mbid_was`, `isrc`, `upc`, `artist`, `duration`, `artist_type`, `original_album` |
| Switch | `[also_on.sources.musicbrainz_entity] enabled`, on by default |

The card of a MusicBrainz page (its title, type, artist credit and the facts MusicBrainz states
about the entity) comes from the web route, not from this source. See
[also on](../api.md#also-on).

## What it does

### Recording, release, release group and artist pages

The page's MBID names the entity. The source reads it (`GET /{kind}/{mbid}`) and records what
`musicbrainz` records for one matched entity, at the
page's level: a recording page lists tracks and videos, a release or release group page lists
albums, an artist page lists artist pages.

- A recording that is one of several agreeing duplicates is keyed by the recording that stands
  for them all ([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)).
- An MBID that MusicBrainz merged into another entity answers with that entity. The page is then
  keyed by the entity it was merged into, and keeps its own MBID as the fact `mbid_was`, so a
  search with `key=mbid:{old}` still finds it.

### An artist's or release group's Wikidata item

An artist page or a release group page also takes the key `wikidata:{Q-id}` of its own Wikidata
item, when all of these hold:

1. The entity's current `wikidata` URL relations name exactly one item. Relations that name
   several items, or none that is an item, give no key.
2. The source reads that item back from Wikidata, and the item is of the entity's kind: an
   artist's item for an artist, a release group's item for a release group. An item that is also
   a work's (a single's song's), one with no MusicBrainz or Discogs ID yet, or one of another
   kind gives no key.
3. The item names this MBID and no other at its level (`P434` for an artist, `P436` for a release
   group). A release group's item must also name no recording (`P4404`).

The page then joins a link that Wikidata names for that item. A page whose item differs from
another member's stays out of that group, because a group holds one item
([groups](../api.md#groups)). A recording page or a release page takes no item key: MusicBrainz
relates items to artists and release groups, and an item a recording or release named would be
its song's or its album's.

### ISRC pages

An ISRC page (`/isrc/{ISRC}`) always keeps the ISRC as the fact `isrc`. What else it records
depends on what MusicBrainz knows:

| MusicBrainz knows | The page records |
| --- | --- |
| No recording with the ISRC | The key `isrc:…` |
| Exactly one recording | The key `isrc:…`, and that recording's keys, facts and links |
| Several recordings that agree (one recording not merged yet) | The key `isrc:…`, the standing recording's key, and the union of the recordings' links and facts |
| Several recordings that disagree | No key: the ISRC names none of them |

An ISRC on recordings that disagree stays a fact. As a key it would join whichever recording's
group took it first.

## Keys, links and facts

Keys, links and facts are the ones listed on the `musicbrainz`
page, plus the Wikidata item key above. Every link is exact and keyed by the entity that names it.

When MusicBrainz's answer is larger than `limits.hub_body_bytes` (8 MiB by default), the source
records only what the page's URL proves: the entity's MBID key, or for an ISRC page the ISRC key
and fact. A recording page keys its own MBID then, even if it is a duplicate another recording
stands for; its group stays apart from theirs until a read succeeds.

## Cost

Each page costs one MusicBrainz request for its entity, and an ISRC page one more for the ISRC. A
recording costs one more request for its original album, and one ISRC lookup per ISRC it carries
(to see whether the ISRC names it alone).
The page's card reads the same entity, so the source usually takes that answer from the cache
(`limits.discovery_cache_secs`, an hour by default).

An artist page or a release group page with a `wikidata` relation costs one Wikidata read of that
item, shared with its card's enrichment through the cache. When Wikidata fails, the
MusicBrainz keys and links stand, the page keeps the item key it had, and the run comes back
after Wikidata's wait. An item that is unknown, gone or larger than the cap gives no key.

## Turn it off

```toml
[also_on.sources.musicbrainz_entity]
enabled = false
```

Its stored keys, links and facts stop showing at once, and MusicBrainz pages join no groups by
them. The pages' own cards still come from MusicBrainz.
