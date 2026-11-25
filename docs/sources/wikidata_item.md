# `wikidata_item`: a Wikidata item link as its own hub

This source runs on links to Wikidata items. It reads the item and lists, as
[also on](../api.md#also-on) links, its entries in other encyclopedias, the music-service pages its
IDs name and its Wikipedia articles, with the item's Q-id as the key. Only the item's own claims
and sitelinks count; nothing is searched.

| | |
| --- | --- |
| Source | `wikidata_item`, also-on source |
| Runs on | Wikidata item links: `https://www.wikidata.org/wiki/{Q-id}` and its [aliases](../api.md#aliases) (`wikidata.org`, `m.wikidata.org`, `/entity/Q…`, `/wiki/Special:EntityPage/Q…`) |
| Asks | Wikidata: `GET https://www.wikidata.org/w/api.php?action=wbgetentities&ids={Q-id}`, with the item's claims and sitelinks |
| Credentials | None |
| Rate | 200 requests a minute on `www.wikidata.org` |
| Rows live | 30 days; rows expire after 60 |
| Keys | `wikidata:Q…` |
| Links | Encyclopedia entries and Wikipedia articles (exact, keyed by the Q-id); music-service pages (exact, unkeyed) |
| Facts | None |
| Switch | `[also_on.sources.wikidata_item] enabled`, on by default |

Property pages (`/wiki/Property:P…`) and `Special:EntityData` links are not item links, and the
source doesn't run on them.

## What it does

The source reads the item and records, in this order:

1. **The key** `wikidata:{Q-id}`.
2. **Entries in other encyclopedias**, from the item's claims (at most two per property, each
   checked against the property's format):

   | Property | Site | Link |
   | --- | --- | --- |
   | `P1417` | Britannica | `https://www.britannica.com/{id}` |
   | `P3123` | Stanford Encyclopedia of Philosophy | `https://plato.stanford.edu/entries/{id}/` |
   | `P5088` | Internet Encyclopedia of Philosophy | `https://iep.utm.edu/{id}/` |
   | `P9526` | Scholarpedia | `http://www.scholarpedia.org/article/{id}` |
   | `P8885` | Namuwiki | `https://namu.wiki/w/{id}` |

3. **Music-service pages** the item's IDs name, at every level (tracks, albums, artists, YouTube
   videos and playlists), up to four per property. The properties are the ones listed on the
   `wikidata` page, without `P2206` (a Discogs release).
4. **Wikipedia articles**, one per language the item has a sitelink for. Major languages come
   first (`en`, `de`, `fr`, `es`, `it`, `ja`, `ru`, `pt`, `zh`, …), then the rest by language
   code.

Only claims at their best rank count (preferred if any, else normal; never deprecated). An item
that Wikidata redirected to another answers as that item, and the link takes that item's key.

## Keys, links and facts

The Q-id key joins the item link to every other link and card that carries `wikidata:{Q-id}`, in
one [group](../api.md#groups).

Encyclopedia entries and Wikipedia articles are keyed by the Q-id, so a tagged link that is one of
them joins the item's group. Music-service pages are not keyed: the item is the thing itself and
names pages at every level, so a song's item lists a track that is one of several recordings.

A card keeps at most `limits.also_on_links_per_source` links from one source (32 by default), in
the order above. On an item with many sitelinks, the less common languages are the ones cut.

## Cost

Each item costs one Wikidata read. The worker reads a batch's items together, up to 10 per
request, and each item's answer is cached as its own read for `limits.discovery_cache_secs` (an
hour by default). A batch that fails, or is larger than `limits.hub_body_bytes`, is read item by
item.

When the item alone is larger than `limits.hub_body_bytes` (8 MiB by default), the source records
only what the link proves: the Q-id key. An unknown or deleted item gives nothing. A busy Wikidata
is retried later, and the link keeps its earlier rows meanwhile.

## Turn it off

```toml
[also_on.sources.wikidata_item]
enabled = false
```

Its stored key and links stop showing at once, and Wikidata item links join no groups by it.
