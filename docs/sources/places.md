# `places`: OpenStreetMap objects, Wikidata items and a place's other pages

This lookup family of [also on](../api.md#also-on) connects OpenStreetMap objects, Wikidata items,
a place's pages on map and review sites, and BTC Map. It matches by exact IDs only: never by name
and never by coordinates.

| | |
| --- | --- |
| Source | `places`, lookup family with sources `osm_wikidata`, `wikidata_osm`, `wikidata_places`, `btcmap` |
| Runs on | `osm_wikidata`: OSM objects tagged `wikidata=Q…`. `wikidata_osm`: Wikidata items, Wikipedia articles and other cards with a `wikidata:` key. `wikidata_places`: place pages on Google Maps, Apple Maps, Yelp and Tripadvisor. `btcmap`: OSM objects |
| Asks | `https://www.wikidata.org/w/api.php`; an article's own Wikipedia (`https://{lang}.wikipedia.org/w/api.php`); `https://api.btcmap.org/v4/places` |
| Credentials | None |
| Rate | At most 200 requests per minute each on `www.wikidata.org` and each `*.wikipedia.org` host; the shared default of 1 per second applies while it is lower. `api.btcmap.org`: shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Rows live | 30 days; rows expire after twice that. `btcmap`: 7 days; rows expire after 14 |
| Keys | `osm:{type}/{id}`, `wikidata:{Q-id}` |
| Links | Wikidata, OpenStreetMap, official website, Google Maps, Apple Maps, Tripadvisor, Yelp, Wikipedia editions, other encyclopedias, BTC Map. All are exact |
| Facts | None |
| Switch | `[also_on.sources.<id>] enabled` for each of the four sources, on by default |

## What it does

A Wikidata value counts at its best rank: the preferred values if there are any, else the normal
ones, and never a deprecated one.

### `osm_wikidata`

1. The card is an OpenStreetMap object whose `wikidata` tag names one item. A tag with several
   (`Q64;Q90`) is not read.
2. The source reads that item. The item must name the object back, by the property for its type:
   P402 (relation), P10689 (way) or P11693 (node). Otherwise the source records nothing. It also
   records nothing when the tag names an item that redirects to another one.
3. The object gets the item's links (below).
4. When the item names no other OSM object, the object also gets two keys, `osm:{type}/{id}` and
   `wikidata:{Q-id}`.

Berlin's item names a relation and a node, so each of the two gets the links without keys.

### `wikidata_osm`

1. The source finds the card's Wikidata item. It comes from a Wikidata item link
   (`www.wikidata.org/wiki/Q…`, `/entity/Q…`), else from a `wikidata:` key another source recorded,
   else from the Wikipedia article's own Wikipedia.
2. The card gets the item's OpenStreetMap object as a link: its relation (P402) when it names
   exactly one, else its only OSM object of any type. An item that names several objects and not
   exactly one relation gives nothing.

It does not run on OpenStreetMap objects.

### `wikidata_places`

The source reads these pages:

| Site | Link | Wikidata property |
| --- | --- | --- |
| Google Maps | `google.com/maps?cid={cid}`, and a place link whose feature ID (`!1s0x{hex}:0x{hex}` in its `data`, or `ftid=0x{hex}:0x{hex}`) holds the CID as its second number, on any of Google's domains | P3749 (the CID in decimal) |
| Apple Maps | `maps.apple.com/place?auid={auid}` | P10046 |
| Yelp | `yelp.com/biz/{name}` | P3108 |
| Tripadvisor | `Attraction_Review-g…-d{number}-….html`, and the same with `Restaurant_Review`, `Hotel_Review` or `ShowUserReviews`, on `tripadvisor.com` or one of its country sites | P3134 (the number after `d`) |

1. The source searches Wikidata for items that carry the page's value. A value with characters
   other than ASCII letters, digits and `._:-/` is not searched.
2. It reads the items back and compares the value byte for byte. Of several items, one that
   another of them replaces (P1366 "replaced by" or P1365 "replaces") gives way to its successor.
   If several still carry the value, or the search has more than five hits, the source records
   nothing.
3. The page gets the item's links (below), without links to the page's own site.
4. When the item carries no other value for the page's site, the page also gets the key
   `wikidata:{Q-id}`.

### `btcmap`

An OpenStreetMap object that BTC Map lists gets its merchant page,
`https://btcmap.org/merchant/{type}:{id}`. The source looks the object up by its OSM ID in BTC
Map's place list. It never probes a merchant page.

## Keys, links and facts

### Keys

| Key | Recorded on | When |
| --- | --- | --- |
| `osm:{type}/{id}` | The OSM object | `osm_wikidata`: object and item name each other, and the item names no other object. `btcmap`: BTC Map lists the object |
| `osm:{type}/{id}` | The BTC Map merchant link | Always, so a tagged merchant page joins the object |
| `wikidata:{Q-id}` | The OSM object | As for `osm_wikidata` above |
| `wikidata:{Q-id}` | A map or review page | The item carries no other value for the page's site |
| `wikidata:{Q-id}` | The links to Wikipedia editions and other encyclopedias | Always, so a tagged article takes the item's key |

Each key names one object, so a [group](../api.md#groups) holds at most one value of each, and
[`key=`](../api.md#search) finds the links that carry it.

The OSM object that `wikidata_osm` and `wikidata_places` link to is never a key. The item's claim
goes one way only, and the object may carry the tag of another item. An object that names the item
back joins through the Q-id that `osm_wikidata` records.

### Links

`osm_wikidata` and `wikidata_places` add an item's links in this order, with `via` `wikidata`:

1. its OpenStreetMap object (`wikidata_places` only, when the item names exactly one);
2. its Wikidata page;
3. its official website, Google Maps, Apple Maps, Tripadvisor and Yelp pages. A property with
   several values adds no link;
4. its Wikipedia editions, the major languages first;
5. its Britannica, Stanford Encyclopedia of Philosophy, Internet Encyclopedia of Philosophy,
   Scholarpedia and Namuwiki entries.

Each source keeps the first `limits.also_on_links_per_source` (32 by default). `wikidata_osm` adds
the OpenStreetMap link only (`via` `wikidata`), and `btcmap` the merchant page only (`via`
`btcmap`).

### Service name

A BTC Map merchant page (`/merchant/{node|way|relation}:{id}`) has `site` `BTC Map`. Other pages on
`btcmap.org` do not.

## Cost

| Source | Requests per card |
| --- | --- |
| `osm_wikidata` | One item read |
| `wikidata_osm` | One item read. For an article without a `wikidata:` key, one request to its Wikipedia first |
| `wikidata_places` | One search and one read of the items it finds. When the search finds several items, one more read |
| `btcmap` | None of its own, while the place list is fresh |

Wikidata and Wikipedia answers are cached for `limits.discovery_cache_secs` (an hour by default) and
shared with the other sources that ask for them. A Wikidata answer over `limits.hub_body_bytes` gives nothing.
A busy Wikidata, Wikipedia or BTC Map fails the source's run, which is retried
([runs](../operations.md#runs)).

`btcmap` keeps BTC Map's place list in memory. A lookup brings it up to date at most once an hour,
from the sync (`updated_since`, deleted places included), 2,000 places per page and up to 25 pages
per lookup. While a first sync needs more pages, the run fails and is retried, and the next lookup
continues the sync. After a restart, the first lookup reads the list again.

## Turn it off

Switch off each source on its own:

```toml
[also_on.sources.osm_wikidata]
enabled = false

[also_on.sources.wikidata_osm]
enabled = false

[also_on.sources.wikidata_places]
enabled = false

[also_on.sources.btcmap]
enabled = false
```

After the restart, a source's stored keys and links stop showing, and groups leave its keys out.
They are kept until they expire and show again when the source is on. Links that other sources
found stay.
