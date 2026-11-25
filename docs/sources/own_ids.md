# `own_ids`: an official record's own ids as its keys

This also-on source runs on cards that an [official source](README.md) read. A record's ids (a DOI, a
Wikidata item, an ISBN, a registry's ID) are facts of its card. This source makes join keys of the
ones the official source declares its own.

| | |
| --- | --- |
| Source | `own_ids`, also-on source |
| Runs on | Cards of an official source that declares ids of its own and is not a page hook, when the card holds such an id |
| Asks | No service |
| Credentials | None |
| Rate | None needed: it sends no request |
| Rows live | 30 days; rows expire after twice that |
| Keys | `doi:{DOI}`, `wikidata:Q…` |
| Links | None |
| Facts | None |
| Switch | `[also_on.sources.own_ids] enabled`, on by default |

## What it does

An official source can declare two kinds of id as its own. Its page says which of its ids join.

| Declared id | Key | Joins the group of |
| --- | --- | --- |
| A DOI the service issued itself, under the prefix it issues its DOIs under (the `doi` fact) | `doi:{DOI}`, in lower case | The `doi:` card of that DOI |
| The Wikidata item the service's own record names for the object (the `wikidata` fact) | `wikidata:Q…` | The item's card |

`via` names the official source.

A fact can hold several values, joined with `; `. It gives keys only when every value is the source's
own, so a typed value that holds a `; ` never yields a part of it.

Examples:

- A repository's record with its own DOI `10.5281/zenodo.7790295` has the key
  `doi:10.5281/zenodo.7790295`, via the source that read it. The record joins the group of
  `doi:10.5281/zenodo.7790295`.
- A record whose DOI is a publisher's, `10.1175/2010mwr3413.1`, has the fact `doi` and no key.
- A museum object whose record names Wikidata item `Q18689458` has the key `wikidata:Q18689458`. The
  object joins the group of `https://www.wikidata.org/wiki/Q18689458`.

## Keys, links and facts

The keys are exact. These ids stay facts and join nothing:

- a DOI under another prefix, such as a publisher's, which the depositor or the metadata typed;
- any id the source does not declare;
- any id on a card a page hook made, because what a page says about itself joins nothing.

A [group](../api.md#groups) holds at most one DOI and one Wikidata item, whichever source recorded
them.

The other cards carry the same keys from other also-on sources. The `doi:` card of a DOI holds it as
its key from `datacite` (except arXiv's own DOIs), and a Wikidata item's card holds its Q-id from
`wikidata_item`. Sources that run after `own_ids` see its keys as they see any other: `wikidata_osm`
reads the Wikidata item and adds the OpenStreetMap object it names, if any.

`key=doi:{DOI}` and `key=wikidata:Q…` find the links that carry these keys.

## Cost

None. The source reads only the stored card.

## Turn it off

```toml
[also_on.sources.own_ids]
enabled = false
```

After the restart its stored keys stop showing: cards, search and groups leave them out. They are
kept until they expire, and show again when the source is on. The ids stay facts of the card.
