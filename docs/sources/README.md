# Sources: cards from official APIs

For provisioning, use the [API-key guide](../api-keys.md): every supported optional credential,
where to obtain it, minimum access and what remains without it. The
[deployment guide](../deployment.md) covers hosting, storage and network requirements.

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
- records itself on the card: a read's `from` names the source and `facts` holds the
  fields its API names for the object, by name (lists joined with `; `).

A few sources are page hooks instead (their entries below say so): they claim no links. The web route
fetches the page as usual, and when the page names its own record on its service (a header or
head tag on the page's own origin), the source reads that record and improves the page's card,
within the web fetch's deadline and at most 3 seconds. The card then names the source in
`from` too; a failure leaves the page's own card.

Two sources are wall hooks (their entries say so), used when a page answers with an access
wall: one looks for that exact link in the same site's RSS or Atom feed or news sitemap, and the
item makes the card; the other reads the exact record of a walled media page.

## Switches

Each source has its own switch, on by default:

```toml
[sources.<name>]
enabled = false
```

(or `PINAKES__SOURCES__<NAME>__ENABLED=false`). A switched-off source's web links get
ordinary web cards; a link in its own identifier scheme (such as `fr:…`) waits until the source is
on again. The whole route can be switched off with `[cards.official] enabled = false`.

## Politeness

Every request carries the operator's contact in the User-Agent (`http.contact`, see
[operations](../operations.md#deployment-and-egress)); while it is empty, the links of a source
whose service requires an email address (its page says so) wait and are retried hourly.
Requests share the per-host budget (`limits.host_requests_per_second`, one per second by
default) and never have more than one request in flight per host. A source whose service
publishes a lower limit lowers that host's rate (a windowed limit as its average rate); a host
whose next request is further away than a fetch may wait is retried later. `limits.host_rates`
overrides both. Each page below names the service's limits, licence and any attribution the card
carries.

After an upgrade that adds a source, run `pinakesd reextract web official`, so links that failed
or got plain web cards before are read by their source.

## Sources

- [`app_store`](app_store.md): App Store apps by ID, from the iTunes Lookup API.
- [`artic`](artic.md): Art Institute of Chicago artworks by id.
- [`bluesky`](bluesky.md): Bluesky posts and profiles.
- [`castro`](castro.md): page hook: a Castro episode's file and show, from its own page.
- [`cleveland`](cleveland.md): Cleveland Museum of Art artworks by accession number.
- [`clinicaltrials`](clinicaltrials.md): ClinicalTrials.gov clinical studies by NCT number.
- [`commons`](commons.md): Wikimedia Commons files (images, video, audio) by file name.
- [`courtlistener`](courtlistener.md): US court opinions by cluster id, with the operator's token (off
  without one)
- [`discogs`](discogs.md): Discogs artists, masters and releases by ID.
- [`discourse`](discourse.md): Topics and posts on any Discourse forum.
- [`docker_hub`](docker_hub.md): Docker Hub image repositories by namespace and name.
- [`ecfr`](ecfr.md): sections and parts of the Code of Federal Regulations.
- [`eur_lex`](eur_lex.md): EU law by CELEX number, ELI or Official Journal id.
- [`facebook`](facebook.md): Facebook pages, posts and videos (events are the event module's).
- [`fdroid`](fdroid.md): F-Droid apps by package, from its package API and page.
- [`federal_register`](federal_register.md): Federal Register documents by document number.
- [`fedlex`](fedlex.md): Swiss federal law in the classified compilation, by ELI.
- [`feed`](feed.md): wall hook: a walled page's card from the exact item in its site's RSS/Atom feed or news sitemap
- [`figshare`](figshare.md): Figshare articles (datasets, figures, papers) by article id.
- [`flathub`](flathub.md): Flathub apps by app ID, from its AppStream API.
- [`forgejo`](forgejo.md): repositories, branches, tags, releases and files on Forgejo and Gitea instances (Codeberg, gitea.com, others)
- [`fountain`](fountain.md): page hook: a Fountain show's or episode's feed, from the data its page embeds.
- [`gbif`](gbif.md): GBIF species and occurrence records by key.
- [`ghost`](ghost.md): page hook: a Ghost post's title, excerpt, image, dates, authors and tags from the Content API
- [`github`](github.md): repositories on github.com, with the operator's token (off without one)
- [`gitlab`](gitlab.md): projects, branches, tags, releases and files on gitlab.com and other GitLab instances
- [`gleif`](gleif.md): Legal Entity Identifier records.
- [`google_play`](google_play.md): page hook: a Google Play app's name, icon, developer and category from its page.
- [`govinfo`](govinfo.md): US public laws, bills, US Code sections and the Statutes at Large.
- [`greenhouse`](greenhouse.md): Greenhouse job board postings by board and job id.
- [`hacker_news`](hacker_news.md): Hacker News items by id (and hnsignals.com pages).
- [`huggingface`](huggingface.md): Hugging Face models, datasets and Spaces by repo id.
- [`inaturalist`](inaturalist.md): iNaturalist observations and taxa by id.
- [`instagram`](instagram.md): Instagram posts, reels and profiles.
- [`internet_archive`](internet_archive.md): Internet Archive items (books, films, audio, software…) by identifier.
- [`legislation_gov_uk`](legislation_gov_uk.md): UK Acts, statutory instruments and retained EU law.
- [`linkedin`](linkedin.md): LinkedIn profiles, company pages and posts.
- [`lobsters`](lobsters.md): Lobsters stories by short id.
- [`mastodon`](mastodon.md): Mastodon posts on any server.
- [`media_records`](media_records.md): page and wall hook: a film's, series', book's or podcast's page named after its object, from its exact record (Wikidata by the page's ID, Open Library, TVmaze, iTunes)
- [`mempool`](mempool.md): Bitcoin blocks, transactions and addresses, and Lightning nodes and
  channels, from mempool.space (Esplora as fallback).
- [`met`](met.md): The Metropolitan Museum of Art's works by object id.
- [`open_food_facts`](open_food_facts.md): products on the Open Food Facts databases by barcode.
- [`pdb`](pdb.md): Protein Data Bank structures by PDB id.
- [`pinterest`](pinterest.md): Pinterest pins and profiles.
- [`podcast_index`](podcast_index.md): Podcast Index shows and episodes by feed and episode id (needs the Podcast Index key).
- [`pubchem`](pubchem.md): PubChem compounds by compound id.
- [`public_inbox`](public_inbox.md): mailing-list messages in public-inbox archives (gnusha.org, lore.kernel.org, others), and old bitcoin-dev pipermail addresses
- [`reddit`](reddit.md): Reddit posts, subreddits and users.
- [`ror`](ror.md): research organizations by ROR id.
- [`sec`](sec.md): SEC EDGAR filings, their documents and companies.
- [`shopify`](shopify.md): Shopify stores' product pages, from the store's own product record (a page
  hook).
- [`snap_store`](snap_store.md): Snap Store apps by snap name, from the snapcraft.io info API.
- [`stack_exchange`](stack_exchange.md): Stack Exchange questions (Stack Overflow and the network's sites) by id.
- [`stacker_news`](stacker_news.md): Stacker News posts and comments by item id.
- [`substack`](substack.md): Substack publications, posts and profiles.
- [`swiss_parliament`](swiss_parliament.md): Swiss Parliament business items (Curia Vista) by affair id.
- [`telegram`](telegram.md): Telegram public channels and their posts.
- [`threads`](threads.md): Threads posts and profiles.
- [`tiktok`](tiktok.md): TikTok videos and profiles.
- [`twitch`](twitch.md): Twitch channels, videos and clips.
- [`uk_parliament_bills`](uk_parliament_bills.md): UK Parliament bills and Acts by bill id.
- [`uniprot`](uniprot.md): UniProtKB proteins by accession.
- [`usgs`](usgs.md): USGS earthquakes and other seismic events by event id.
- [`wordpress`](wordpress.md): page hook: a WordPress post's editorial title, excerpt, dates and featured image from its REST record
- [`x`](x.md): X (Twitter) posts and profiles.
- [`zefix`](zefix.md): Swiss companies in the commercial register, by register id.
- [`zenodo`](zenodo.md): Zenodo records (datasets, software, papers) by record id.

## Also on

The [also-on](../api.md#also-on) sources, in the order they run: each reads a resource after it
is saved and adds keys, facts and derived links (the API says how they show). A lookup family is
one module with its sources; a name that is also an official source above has its also-on part
on the same page. A source that records keys lists their forms (`Source::key_forms`, with an
example each): every form searches back as recorded (`GET /v1/search?key=`, tested for every
registered source), and in a debug build a source records no other form.

### An ISRC on several recordings

MusicBrainz gives one ISRC to several recordings at times. When those recordings agree, they
are one recording MusicBrainz has not merged yet: the same artists (their MBIDs), the same title
once the tags that keep a recording's identity are dropped (a remaster or a version tag goes; a
live, remix or edit tag tells recordings apart), the same disambiguation (MusicBrainz keeps a
live recording's title bare and says "live, …" there), all audio or all video, and every two
known lengths within 3 s. Then the
ISRC is the track's key and so is the MBID of the one that stands for them all (the smallest
MBID, `mb:recording:…`, `via` the source; a group holds one recording MBID, so the others are
facts), a link MusicBrainz relates to any of them is keyed by that one too, and `musicbrainz`
takes the union of their links and facts; another ISRC they carry is a key only when it names
the same recording on its own (one shared with a recording that disagrees stays a fact). The set
is the whole connected set a recording's ISRCs reach (a duplicate that shares one ISRC with one
recording and another with a third makes all three one), and it must agree as a whole (lengths
drifting 3 s at each step of a chain are not one recording), read through MusicBrainz's own
records of the exact ISRCs: at most 8 recordings, 16 ISRC lookups and 32 recording reads per
walk, all cached; past that, when MusicBrainz cannot serve a member, or when the whole set does
not agree, today's rule applies. Every path that keys a recording goes through the one function
(`music_sources::recording_key`): a service's own ISRC and the duration's pick, a URL relation, a
recording or ISRC page, Wikidata's recording item (whose replaced MBID stays as `mbid_was`); only a
recording page MusicBrainz answers over its cap keys its own MBID, as its link alone proves.
Sources that still key different recordings for one resource truly disagree: both keys stay and
the resource joins no recording group, as for any conflicting key; such runs are counted
(`pinakes_also_on_recording_key_conflicts_total`) and nothing is dropped. A MusicBrainz merge, or a new duplicate with a
smaller MBID, changes the recording that stands for a set: rows heal at each resource's next run
(within the sources' 30 days; `reextract also-on` sooner). When they disagree, the ISRC alone names none
of them and the track's duration decides: the one recording whose length is within 3 s of it,
while every other's length is known and is not, is the track's (its MBID the key, the ISRC a
fact); when none or two are that close, or the page states no duration, the ISRC is a fact only
(a key would join whichever recording's group took it first). Only MusicBrainz's own data for the
exact ISRC decides, never a title search. Each music source that reads ISRCs follows this rule,
so one goes without the others needing a change.

- [`service`](service.md): the ISRC a music service's own track page states.
- [`tidal`](tidal.md): TIDAL's own barcodes and ISRCs (with the operator's TIDAL credentials).
- [`spotify`](spotify.md): Spotify's own barcodes and ISRCs (with the operator's Spotify app credentials).
- [`musicbrainz`](musicbrainz.md): what MusicBrainz relates a music link to.
- [`musicbrainz_entity`](musicbrainz_entity.md): a MusicBrainz page as its own hub.
- [`musicbrainz_containment`](musicbrainz_containment.md): an exact recording's releases as containment facts, never identity edges.
- [`performers`](performers.md): an event's performers, keyed to the artist on the page's own service.
- [`wikidata`](wikidata.md): the Wikidata item of a music link.
- [`discogs`](discogs.md#also-on): a Discogs object's key and a release's barcodes.
- [`deezer`](deezer.md): Deezer's own ISRCs and barcodes, and another service's track or album on Deezer by its ISRC or barcode.
- [`spotify_isrc`](spotify_isrc.md): another service's track or album on Spotify by its ISRC or barcode (with the operator's Spotify app credentials).
- [`tidal_isrc`](tidal_isrc.md): another service's track or album on TIDAL by its ISRC or barcode (with the operator's TIDAL credentials).
- [`itunes`](itunes.md): Apple Music albums by barcode, and an Apple Music track's preview.
- [`itunes_song`](itunes_song.md): Apple Music's song for another service's track, by its album's barcode, place and title (a maybe: a match, unkeyed).
- [`youtube_match`](youtube_match.md): a label's YouTube upload on Deezer and TIDAL, by artist, title and length (a maybe: a match, unkeyed).
- [`wikipedia`](wikipedia.md): the other editions of a Wikipedia article.
- [`wikidata_item`](wikidata_item.md): a Wikidata item link as its own hub.
- [`books`](books.md): Open Library, Amazon, Apple Books and Wikidata for a book.
- [`films`](films.md): the IMDb title of a film or series.
- [`tmdb`](tmdb.md): TMDB's poster and overview for a film or series card (with the operator's TMDB credential).
- [`podcasts`](podcasts.md): Apple Podcasts, Podcast Index and Wikidata for a show.
- [`product_keys`](product_keys.md): a product's own barcode as its key.
- [`open_food_facts`](open_food_facts.md#also-on): a product's barcode on Open Food, Beauty,
  Products and Pet Food Facts.
- [`stack_exchange`](stack_exchange.md#also-on): a question's id, and the original of a duplicate.
- [`media_records`](media_records.md#also-on): `titles`, the title of a media page the page hook
  could not name in time.
- [`federal_register`](federal_register.md#also-on): a Federal Register document's GovInfo,
  regulations.gov and public-inspection copies.
- [`mempool`](mempool.md#also-on): a Bitcoin block, transaction or address, or a Lightning
  node or channel, on the other block explorers.
- [`places`](places.md): `osm_wikidata`, `wikidata_osm`, `wikidata_places` and `btcmap`,
  OpenStreetMap objects and Wikidata items that name each other, and a place's map and review
  pages through its item.
- [`papers`](papers.md): `unpaywall`, `ncbi`, `datacite` and `huggingface`, a paper's open copies,
  PubMed records and preprint.
- [`packages`](packages.md): `crates`, `npm`, `pypi` and `software_heritage`, a package's registry
  pages, provenance and archive.
- [`games`](games.md): `steam` and `wikidata_stores`, a game's other stores.
- [`apps`](apps.md): `app_stores`, an app's other stores, bridged by Wikidata.
- [`archive`](archive.md): `wayback`, an archived copy of a gone page.
- [`editions`](editions.md): which release each link of a recording or an album is on (last).

## Removing a source

Every source (official, also-on, or a lookup family) is removed by deleting its own files and
its registry lines, nothing else; `scripts/test` and `scripts/e2e --mock` then pass as they are:

- its module: `crates/pinakes-core/src/sources/<name>.rs` (an official source, with its also-on
  half when it has one), `crates/pinakes-core/src/also_on/<id>.rs` or
  `crates/pinakes-core/src/lookups/<family>.rs` (a family, with its also-on sources);
- its lines in each registry it is in: `pub mod <name>;` with its entry in `sources::REGISTRY`,
  `also_on::registry()` (also for an official source's also-on half) or `lookups::FAMILIES`;
- its page `docs/sources/<name>.md` and its lines in this index;
- its `[sources.<name>]` block, `[also_on.sources.<id>]` lines and own section (`[<name>]`) in
  `pinakes.example.toml`, and its banner paragraph and variables in `.env.example` if it has a
  secret;
- its fixture directories, those named after it:
  `crates/pinakes-core/tests/fixtures/sources/<name>/`,
  `crates/pinakes-core/tests/fixtures/media/<name>/` or
  `crates/pinakes-core/tests/fixtures/lookups/<family>/` (recordings several modules read are in
  `shared/` there, never in a directory named after a source), and its own test file
  (`crates/pinakes-core/tests/<name>.rs` or `tests/also_on_<name>.rs`) if it has one;
- its live end-to-end samples, `e2e/samples/<name>.json`.

`scripts/check-removals [--tests] [NAME…]` proves it: it removes each source on a scratch
worktree by this list, checks that nothing of the source's own is left (it looks for the
source's id and display name where every source has lines of its own, parses the example
configuration and `.env.example`, and finds a link to its page anywhere, independently of what
it deleted; prose elsewhere that names the source is listed for a reader), builds every test
target with warnings as errors, and with `--tests`
runs the workspace's tests too, which catch what the build cannot (a test of another source that
relied on it, a recording it took along). A removal that would need any other edit is named
before it is needed.

Shared code stays (`sources/kit.rs`, `sources/biodiversity.rs`, `music.rs`, `music_sources.rs`,
`wikipedia.rs`, `lookups/mod.rs`, `lookups/wikidata.rs`, `also_on/media.rs`,
`also_on/client_credentials.rs`, and what several sources ask one service with, in
`also_on/*_api.rs`); code only one source uses is in that source's files, so a removal leaves no
unused code behind (test helpers for any source excepted). A source that needs to know whether
another one runs asks by its ID (`also_on::runs_by_id`, `also_on::enabled_by_id`), never by its
module. Tests that need a source skip without it, so none has to change. The
keys a removed source recorded stay until they expire (up to their lifetime), and keep their
single-object rule: the daemon writes every registered namespace's rule to the database at start
(`single_object_namespace`), so grouping reads it after the code is gone.
