# Sources: cards from official APIs

Some services answer exactly from their own API where their page is walled, thin or has moved.
For their links, Pinakes reads that API instead of the page. Every source here:

- reads one object by an identifier taken from the link, never a search by title. A UniProt
  entry name is UniProt's own identifier. A PubChem compound name gets the compound's card when
  PubChem names exactly one compound for it, but is never an alias of it (nothing merges on a
  name); a name with several compounds gets an ordinary web card;
- needs no key unless its page says it takes the operator's token (a source that needs one is
  off without it, one that takes an optional one does more with it);
- makes every address of the same object one resource: the other addresses are
  [aliases](../api.md#aliases) of one root;
- records itself on the card: `details.from` names the source and `details.facts` holds the
  fields its API names for the object, by name (lists joined with `; `).

A few sources are page hooks instead (WordPress, Ghost, media records): they claim no links. The web route
fetches the page as usual, and when the page names its own record on its service (a header or
head tag on the page's own origin), the source reads that record and improves the page's card,
within the web fetch's deadline and at most 3 seconds. The card then names the source in
`details.from` too; a failure leaves the page's own card.

Two sources are wall hooks, used when a page answers with an access wall: [`feed`](feed.md) looks
for that exact link in the same site's RSS or Atom feed or news sitemap, and the item makes the
card; [`media_records`](media_records.md) reads the exact record of a walled IMDb title or
podcast page.

## Switches

Each source has its own switch, on by default:

```toml
[sources.federal_register]
enabled = false
```

(or `PINAKES__SOURCES__FEDERAL_REGISTER__ENABLED=false`). A switched-off source's web links get
ordinary web cards; a link in its own identifier scheme (such as `fr:…`) waits until the source is
on again. The whole route can be switched off with `[cards.official] enabled = false`.

## Politeness

Every request carries the operator's contact in the User-Agent (`http.contact`, see
[operations](../operations.md#deployment-and-egress)); while it is empty, the links of a source
whose service requires an email address ([SEC EDGAR](sec.md)) wait and are retried hourly.
Requests share the per-host budget (`limits.host_requests_per_second`, one per second by
default) and never have more than one request in flight per host. A source whose service
publishes a lower limit lowers that host's rate (a windowed limit as its average rate); a host
whose next request is further away than a fetch may wait is retried later. `limits.host_rates`
overrides both. Each page below names the service's limits, licence and any attribution the card
carries.

After an upgrade that adds a source, run `pinakesd reextract web official`, so links that failed
or got plain web cards before are read by their source.

## Sources

- [`artic`](artic.md): Art Institute of Chicago artworks by id.
- [`bluesky`](bluesky.md): Bluesky posts and profiles.
- [`cleveland`](cleveland.md): Cleveland Museum of Art artworks by accession number.
- [`clinicaltrials`](clinicaltrials.md): ClinicalTrials.gov clinical studies by NCT number.
- [`commons`](commons.md): Wikimedia Commons files (images, video, audio) by file name.
- [`courtlistener`](courtlistener.md): US court opinions by cluster id.
- [`discogs`](discogs.md): Discogs artists, masters and releases by ID.
- [`discourse`](discourse.md): Topics and posts on any Discourse forum.
- [`docker_hub`](docker_hub.md): Docker Hub image repositories by namespace and name.
- [`ecfr`](ecfr.md): sections and parts of the Code of Federal Regulations.
- [`eur_lex`](eur_lex.md): EU law by CELEX number, ELI or Official Journal id.
- [`federal_register`](federal_register.md): Federal Register documents by document number.
- [`fedlex`](fedlex.md): Swiss federal law in the classified compilation, by ELI.
- [`feed`](feed.md): wall hook: a walled page's card from the exact item in its site's RSS/Atom feed or news sitemap
- [`figshare`](figshare.md): Figshare articles (datasets, figures, papers) by article id.
- [`forgejo`](forgejo.md): repositories, branches, tags, releases and files on Forgejo and Gitea instances (Codeberg, gitea.com, others)
- [`gbif`](gbif.md): GBIF species and occurrence records by key.
- [`ghost`](ghost.md): page hook: a Ghost post's title, excerpt, image, dates, authors and tags from the Content API
- [`github`](github.md): repositories on github.com, with the operator's token (off without one)
- [`gitlab`](gitlab.md): projects, branches, tags, releases and files on gitlab.com and other GitLab instances
- [`gleif`](gleif.md): Legal Entity Identifier records.
- [`govinfo`](govinfo.md): US public laws, bills, US Code sections and the Statutes at Large.
- [`hacker_news`](hacker_news.md): Hacker News items by id (and hnsignals.com pages).
- [`huggingface`](huggingface.md): Hugging Face models, datasets and Spaces by repo id.
- [`inaturalist`](inaturalist.md): iNaturalist observations and taxa by id.
- [`internet_archive`](internet_archive.md): Internet Archive items (books, films, audio, software…) by identifier.
- [`legislation_gov_uk`](legislation_gov_uk.md): UK Acts, statutory instruments and retained EU law.
- [`lobsters`](lobsters.md): Lobsters stories by short id.
- [`mastodon`](mastodon.md): Mastodon posts on any server.
- [`media_records`](media_records.md): page and wall hook: a film's, series', book's or podcast's page named after its object, from its exact record (Wikidata by the page's ID, Open Library, TVmaze, iTunes)
- [`mempool`](mempool.md): Bitcoin blocks, transactions and addresses, and Lightning nodes and
  channels, from mempool.space (Esplora as fallback).
- [`met`](met.md): The Metropolitan Museum of Art's works by object id.
- [`pdb`](pdb.md): Protein Data Bank structures by PDB id.
- [`podcast_index`](podcast_index.md): Podcast Index shows and episodes by feed and episode id (needs the Podcast Index key).
- [`pubchem`](pubchem.md): PubChem compounds by compound id.
- [`public_inbox`](public_inbox.md): mailing-list messages in public-inbox archives (gnusha.org, lore.kernel.org, others), and old bitcoin-dev pipermail addresses
- [`ror`](ror.md): research organizations by ROR id.
- [`sec`](sec.md): SEC EDGAR filings, their documents and companies.
- [`stacker_news`](stacker_news.md): Stacker News posts and comments by item id.
- [`swiss_parliament`](swiss_parliament.md): Swiss Parliament business items (Curia Vista) by affair id.
- [`uk_parliament_bills`](uk_parliament_bills.md): UK Parliament bills and Acts by bill id.
- [`uniprot`](uniprot.md): UniProtKB proteins by accession.
- [`usgs`](usgs.md): USGS earthquakes and other seismic events by event id.
- [`wordpress`](wordpress.md): page hook: a WordPress post's editorial title, excerpt, dates and featured image from its REST record
- [`zefix`](zefix.md): Swiss companies in the commercial register, by register id.
- [`zenodo`](zenodo.md): Zenodo records (datasets, software, papers) by record id.

## Also on

The [also-on](../api.md#also-on) sources, in the order they run: each reads a resource after it
is saved and adds keys, facts and derived links (the API says how they show). A lookup family is
one module with its sources; a name that is also an official source above has its also-on part
on the same page.

- [`service`](service.md): the ISRC a music service's own track page states.
- [`tidal`](tidal.md): TIDAL's own barcodes and ISRCs (with the operator's TIDAL credentials).
- [`musicbrainz`](musicbrainz.md): what MusicBrainz relates a music link to.
- [`musicbrainz_entity`](musicbrainz_entity.md): a MusicBrainz page as its own hub.
- [`performers`](performers.md): an event's performers, keyed to the artist on the page's own service.
- [`wikidata`](wikidata.md): the Wikidata item of a music link.
- [`discogs`](discogs.md#also-on): a Discogs object's key and a release's barcodes.
- [`deezer`](deezer.md): Deezer's own ISRCs and barcodes (off by default).
- [`itunes`](itunes.md): Apple Music albums by barcode.
- [`wikipedia`](wikipedia.md): the other editions of a Wikipedia article.
- [`wikidata_item`](wikidata_item.md): a Wikidata item link as its own hub.
- [`books`](books.md): Open Library, Amazon, Apple Books and Wikidata for a book.
- [`films`](films.md): the IMDb title of a film or series.
- [`podcasts`](podcasts.md): Apple Podcasts, Podcast Index and Wikidata for a show.
- [`media_records`](media_records.md#also-on): `titles`, the title of a media page the page hook
  could not name in time.
- [`federal_register`](federal_register.md#also-on): a Federal Register document's GovInfo,
  regulations.gov and public-inspection copies.
- [`places`](places.md): `osm_wikidata`, `wikidata_osm` and `btcmap`, OpenStreetMap objects and
  Wikidata items that name each other.
- [`papers`](papers.md): `unpaywall`, `ncbi`, `datacite` and `huggingface`, a paper's open copies,
  PubMed records and preprint.
- [`packages`](packages.md): `crates`, `npm`, `pypi` and `software_heritage`, a package's registry
  pages, provenance and archive.
- [`games`](games.md): `steam` and `wikidata_stores`, a game's and an app's other stores.
- [`archive`](archive.md): `wayback`, an archived copy of a gone page.

## Removing a source

Every source (official, also-on, or a lookup family) is removed by deleting its own files and
its registry lines, nothing else; `scripts/test` and `scripts/e2e --mock` then pass as they are:

- its module: `crates/pinakes-core/src/sources/<name>.rs` (an official source, with its also-on
  half when it has one), `crates/pinakes-core/src/also_on/<id>.rs` or
  `crates/pinakes-core/src/lookups/<family>.rs` (a family, with its also-on sources);
- its lines in each registry it is in: `pub mod <name>;` with its entry in `sources::REGISTRY`,
  `also_on::registry()` (also for an official source's also-on half) or `lookups::FAMILIES`;
- its page `docs/sources/<name>.md` and its lines in this index;
- its `[sources.<name>]` block, `[also_on.sources.<id>]` lines and own section (such as
  `[tidal]`) in `pinakes.example.toml`, and its variables in `.env.example` if it has a secret;
- its fixtures, `crates/pinakes-core/tests/fixtures/sources/<name>/`,
  `crates/pinakes-core/tests/fixtures/media/<name>/` or
  `crates/pinakes-core/tests/fixtures/lookups/<family>/`, and its own test file
  (`crates/pinakes-core/tests/<name>.rs` or `tests/also_on_<name>.rs`) if it has one;
- its live end-to-end samples, `e2e/samples/<name>.json`.

`scripts/check-removals [NAME…]` proves it: it removes each source on a scratch worktree by this
list and builds every test target with warnings as errors, so a removal that would need any other
edit is named before it is needed.

Shared code stays (`sources/kit.rs`, `sources/biodiversity.rs`, `music.rs`, `music_sources.rs`,
`wikipedia.rs`, `lookups/mod.rs`, `lookups/wikidata.rs`, `also_on/media.rs`), even a part only
the removed source used. Tests that need a source skip without it, so none has to change.
