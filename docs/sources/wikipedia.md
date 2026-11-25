# `wikipedia`: the other editions of a Wikipedia article

This source runs on Wikipedia articles. It asks the article's own Wikipedia for its Wikidata item,
then lists the item's articles in other languages and its entries in other encyclopedias as
[also on](../api.md#also-on) links, with the Q-id as the key. Titles are never matched across
languages.

| | |
| --- | --- |
| Source | `wikipedia`, also-on source |
| Runs on | Wikipedia article links: `https://{lang}.wikipedia.org/wiki/{Title}` and its [aliases](../api.md#aliases) (the mobile host `{lang}.m.wikipedia.org`, `/w/index.php?title=…`) |
| Asks | The article's Wikipedia: `GET https://{lang}.wikipedia.org/w/api.php?action=query&prop=pageprops&ppprop=wikibase_item&titles={Title}&redirects=1`; then Wikidata: `GET https://www.wikidata.org/w/api.php?action=wbgetentities&ids={Q-id}` |
| Credentials | None |
| Rate | 200 requests a minute on each `*.wikipedia.org` host and on `www.wikidata.org` |
| Rows live | 30 days; rows expire after 60 |
| Keys | `wikidata:Q…` |
| Links | The item's Wikipedia articles in other languages and its encyclopedia entries, exact (keyed by the Q-id) |
| Facts | None |
| Switch | `[also_on.sources.wikipedia] enabled`, on by default |

The article's card itself (its title, first paragraph and image) comes from its Wikipedia's REST
summary, not from this source.

## What it does

1. The source asks the article's own Wikipedia which Wikidata item the article has, following
   redirects. A redirect's title stays its own link, and takes the item of the article it
   redirects to.
2. It reads that item from Wikidata.
3. It records:
   - the key `wikidata:{Q-id}`, so two editions of one article join one
     [group](../api.md#groups);
   - the item's entries in other encyclopedias: Britannica, Stanford Encyclopedia of Philosophy,
     Internet Encyclopedia of Philosophy, Scholarpedia and Namuwiki (the properties are listed on
     the `wikidata_item` page);
   - the item's Wikipedia articles in every other language, as `Wikipedia ({lang})`. Major
     languages come first (`en`, `de`, `fr`, `es`, `it`, `ja`, `ru`, `pt`, `zh`, …), then the rest
     by language code. The article's own language is left out.

An article its Wikipedia doesn't have, or one without a Wikidata item, gives nothing.

## Keys, links and facts

Every link is exact and keyed by the Q-id, so a tagged link that is one of them joins the
article's group. A card keeps at most `limits.also_on_links_per_source` links from one source
(32 by default): the encyclopedias first, then the languages in the order above.

Unlike `wikidata_item`, this source lists no music-service pages.

## Cost

Each article costs one request to its Wikipedia and one to Wikidata. Both answers are cached for
`limits.discovery_cache_secs` (an hour by default).

- A Wikipedia that refuses, is busy (429 or 5xx) or answers with an API error is retried later.
  The article keeps its earlier rows meanwhile.
- When the item is larger than `limits.hub_body_bytes` (8 MiB by default), the article keeps
  only the key its own Wikipedia proved, `wikidata:{Q-id}`.
- A busy Wikidata is retried later, as above.

## Turn it off

```toml
[also_on.sources.wikipedia]
enabled = false
```

Its stored key and links stop showing at once, and Wikipedia articles join no groups by it.
