# Wikidata dump sample

`dump-sample.json.bz2` is shaped as Wikidata's JSON dump (`[`, one entity per line with a trailing
comma, `]`), compressed with bzip2 at level 1 (100 kB blocks, so it has about 40 blocks):

- 28 entities as they are in the dump `wikidata-20261123-all.json.bz2`
  (`https://dumps.wikimedia.org/wikidatawiki/entities/20261123/`), read on 2026-11-26 from the
  first 8 MB of the file: the 25 smallest of its first 957 entities (one has no Wikipedia article,
  Q5063), a list (Q2041), a disambiguation page (Q3263) and a category (Q4829).
- 14 entities read from the API (`wbgetentities`) on 2026-11-26 at 02:38Z, with the dump's fields
  only (the API's `pageid`, `ns`, `title` and `modified` left out): two genres (Q11366, then the
  cards naming it; Q11399 and Q24925 after the cards naming them), an album (Q202996), a band
  (Q44190), a person (Q42), a film (Q83495), a city (Q72), a company (Q312), a taxon (Q140), an
  app (Q171477), a video game (Q23013817), a novel (Q208460) and a property (P136).

Wikidata's data is CC0 (public domain dedication).

`films.json` holds 9 items read from the API (`wbgetentities`) on 2026-11-26 at 18:58Z, with the
dump's fields only (the API's `pageid`, `ns`, `title`, `lastrevid` and `modified` left out), by
the class they are of (their first `P31`): the smallest of a search's first 8 items of the class
with an IMDb id (`haswbstatement:P31=<class> haswbstatement:P345`) and an English article. An
animated feature film (Q137850120), a short film (Q3179389), an animated short film (Q1412540), a
silent film (Q2945999), a miniseries (Q4993680), an animated television series (Q15106029), a
television program (Q48557), a web series (Q2705899) and a television season (Q3005019).

`no-sitelinks.json` is the API's whole answer (`wbgetentities`, `props=info|sitelinks`,
`formatversion=2`, as the update asks first) for a scholarly article without any sitelink
(Q26778405), read on 2026-11-26 at 22:20Z (`lastrevid` left out): its `sitelinks` is `{}`.
