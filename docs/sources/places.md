# `places`: OpenStreetMap objects, Wikidata items and a place's other pages

A lookup family of [also on](../api.md#also-on) (`lookups/places.rs`).

Only where an OpenStreetMap object and a Wikidata item point at each other:
- `osm_wikidata`: an OSM object tagged `wikidata=Q…` whose item names it back (P402 relation,
  P11693 node or P10689 way) gets the item's Wikidata page, official website, Google Maps, Apple
  Maps, Tripadvisor and Yelp places, and its Wikipedia editions (the major languages first).
  `osm:{type}/{id}` and the Q-id are keys only when the item names no other OSM object, so the
  object and the item's Wikipedia articles join one to one; a city whose item names both its
  relation and a node (Berlin, Lugano) gets the links without keys. An item that does not name
  the object gives nothing.
- `wikidata_osm`: a Wikidata item link (`www.wikidata.org/wiki/Q…`), a Wikipedia article (through
  its item) or a resource another source keyed with a Q-id gets the item's OpenStreetMap object:
  its relation (P402) when it has exactly one, else its only OSM link of any type; several give
  nothing.
- `wikidata_places`: a place's page on Google Maps (`google.com/maps?cid={cid}`), Apple Maps
  (`maps.apple.com/place?auid={auid}`), Yelp (`yelp.com/biz/{name}`) or Tripadvisor (an
  attraction, restaurant or hotel page on any of its country sites, by the `d{number}` in its
  name, which is all Wikidata's P3134 holds) gets the one Wikidata item carrying that value
  (read back and compared exactly; several items are ambiguous and give nothing): the item's
  Q-id as key (when the item carries one value for the page's site), its OSM object as key and link when it names exactly one (so the page joins the
  object's group), and its Wikidata page, other place links and Wikipedia editions.
- `btcmap`: an OSM object that BTC Map lists gets its merchant page
  (`btcmap.org/merchant/{type}:{id}`), from BTC Map's place list by OSM ID, never by probing a
  page. The page names that one object, so both carry the object's `osm:{type}/{id}` key, and a
  tagged merchant link joins the object.

## Operations

Each tagged OSM object costs one Wikidata request; each map or review page two (the search and
the item). `btcmap` keeps BTC Map's place list in
memory, brought up to date at most hourly from its sync (`updated_since`, 2,000 places a page);
after a restart the first lookup reads it again.
