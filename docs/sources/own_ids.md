# `own_ids`: an official record's own ids as its keys

An [also on](../api.md#also-on) source.

A card an [official source](README.md) reads holds the ids its service's record states for the
object (a DOI, a Wikidata item, an ISBN, a registry's id) as facts. `own_ids` makes keys of the
ones the source declares its own, so the card joins a [group](../api.md#groups) with the other
links of the same object:

- a DOI the service issued itself, under the prefix it issues its DOIs under: the key
  `doi:{DOI}` (lower case), and the card joins the group of the `doi:` card of that DOI;
- the Wikidata item the service's own record names for the object: the key `wikidata:Q…`, and the
  card joins the group of the item's card.

`via` names the official source. Each source's page says which of its ids join. Every other id
stays a fact and joins nothing: a DOI the depositor or the metadata typed (a publisher's DOI, under
another prefix), an id the source does not declare, and any id on a card a page hook made (what a
page says about itself). A group holds one DOI and one Wikidata item at most, as for every source.
It asks no service.

`key=doi:{DOI}` and `key=wikidata:Q…` search these keys as they search every other source's.

## Examples

- A repository's record whose own DOI is `10.5281/zenodo.7790295`: key
  `doi:10.5281/zenodo.7790295`, via the source that read it. The record joins the group of
  `doi:10.5281/zenodo.7790295`.
- A record whose DOI is a publisher's, `10.1175/2010mwr3413.1`: `facts.doi`, no key.
- A museum object whose record names Wikidata item `Q18689458`: key `wikidata:Q18689458`; the
  object joins the group of `https://www.wikidata.org/wiki/Q18689458`.
