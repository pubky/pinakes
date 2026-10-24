# `wikidata_item`: a Wikidata item link as its own hub

An [also on](../api.md#also-on) source.

`wikidata_item`: a Wikidata item link (`/wiki/Q…`, `/entity/Q…`,
`Special:EntityPage`, the mobile host: all aliases of `https://www.wikidata.org/wiki/{Q-id}`) is
its own hub. Its card lists the item's encyclopedia entries, the music services its best-rank
IDs name, and all its Wikipedia editions (the major languages first), with the Q-id as key. Only
the item's own claims and sitelinks count, never a search.
