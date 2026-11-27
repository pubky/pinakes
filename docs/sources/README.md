# Sources

A source reads one kind of object from the service that holds it. Pinakes has four kinds:

- **Official sources** read a link's object from its service's own API instead of its page, for
  services whose pages are walled, thin or moved, or whose API says more. The link is routed to the
  source by its shape alone.
- **Page hooks** and **wall hooks** are official sources that claim no links. A page hook improves
  the card of a page the web route fetched, when the page names its own record on its service. A
  wall hook makes a card for a page that answered with an access wall, from what the same site
  publishes elsewhere.
- **Also-on sources** run after a card is saved. They ask hubs and services about the card's object
  and record keys, facts and derived links ([Also on](../api.md#also-on)).
- **Lookup families** are modules of several also-on sources for one domain (places, papers,
  packages, games, apps, archived copies).

Every source is on by default and has its own switch. For credentials, see the
[API-key guide](../api-keys.md); for how cards, keys and groups work in general, see
[How Pinakes works](../design.md).

## How a source page is organized

Each page starts with a summary table, then the details:

| Section | What it says |
| --- | --- |
| Summary table | Links handled, what is fetched, credentials, rate, refresh, card types, keys, credit, switch |
| Links | Every link form the source reads, the root its aliases fold into, and what it does not read |
| What it fetches | Each request, when it is made, and which answers make a card `missing` |
| Card | The card type, the fields it fills and a table of its facts |
| Keys and groups | The keys it records, what joins a group and what stays a fact |
| Credit | What the card credits |
| Turn it off | The switch and what its links get without the source |
| Examples | Links that resolve to a card of each kind |

Also-on and family pages say which cards they run on, what they ask, the keys, links and facts
they record, and what a lookup costs.

## Official sources

### Music, film and podcasts

| Source | Reads | Credentials |
| --- | --- | --- |
| [`castro`](castro.md) | Page hook: a Castro episode's file and show, from its own page | None |
| [`commons`](commons.md) | Wikimedia Commons files (images, video, audio) by file name | None |
| [`discogs`](discogs.md) | Discogs artists, masters and releases by ID | Optional Discogs token or consumer key and secret (images) |
| [`fountain`](fountain.md) | Page hook: a Fountain show's or episode's feed, from the data its page embeds | None |
| [`internet_archive`](internet_archive.md) | Internet Archive items (books, films, audio, software) by identifier | None |
| [`media_records`](media_records.md) | Page and wall hook: a film's, series', book's or podcast's page, from its exact record (Wikidata, Open Library, TVmaze, iTunes) | None |
| [`podcast_index`](podcast_index.md) | Podcast Index shows and episodes by feed and episode ID | Podcast Index key and secret; off without them |

### Social networks

| Source | Reads | Credentials |
| --- | --- | --- |
| [`bluesky`](bluesky.md) | Bluesky posts and profiles | None |
| [`facebook`](facebook.md) | Facebook pages, posts and videos (events belong to the event route) | None |
| [`instagram`](instagram.md) | Instagram posts, reels and profiles | None |
| [`linkedin`](linkedin.md) | LinkedIn profiles, company pages and posts | None |
| [`mastodon`](mastodon.md) | Mastodon posts on any server | None |
| [`pinterest`](pinterest.md) | Pinterest pins and profiles | None |
| [`reddit`](reddit.md) | Reddit posts, subreddits and users | None |
| [`telegram`](telegram.md) | Telegram public channels and their posts | None |
| [`threads`](threads.md) | Threads posts and profiles | None |
| [`tiktok`](tiktok.md) | TikTok videos and profiles | None |
| [`twitch`](twitch.md) | Twitch channels, videos and clips | None |
| [`x`](x.md) | X (Twitter) posts and profiles | None |

### Forums, news and blogs

| Source | Reads | Credentials |
| --- | --- | --- |
| [`discourse`](discourse.md) | Topics and posts on any Discourse forum | None |
| [`feed`](feed.md) | Wall hook: a walled page's card from the exact item in its site's RSS or Atom feed or news sitemap | None |
| [`ghost`](ghost.md) | Page hook: a Ghost post's title, excerpt, image, dates, authors and tags from its Content API | None |
| [`hacker_news`](hacker_news.md) | Hacker News items by ID | None |
| [`lobsters`](lobsters.md) | Lobsters stories by short ID | None |
| [`public_inbox`](public_inbox.md) | Mailing-list messages in public-inbox archives (gnusha.org, lore.kernel.org and others), and old bitcoin-dev pipermail addresses | None |
| [`stack_exchange`](stack_exchange.md) | Stack Exchange questions (Stack Overflow and the network's sites) by ID | Optional app key (a higher quota) |
| [`stacker_news`](stacker_news.md) | Stacker News posts and comments by item ID | None |
| [`substack`](substack.md) | Substack publications, posts and profiles | None |
| [`wordpress`](wordpress.md) | Page hook: a WordPress post's title, excerpt, dates and featured image from its REST record | None |

### Code and software

| Source | Reads | Credentials |
| --- | --- | --- |
| [`docker_hub`](docker_hub.md) | Docker Hub image repositories by namespace and name | None |
| [`forgejo`](forgejo.md) | Repositories, branches, tags, releases and files on Forgejo and Gitea instances (Codeberg, gitea.com and others) | None |
| [`github`](github.md) | Repositories on github.com | Token; off without one |
| [`gitlab`](gitlab.md) | Projects, branches, tags, releases and files on gitlab.com and other GitLab instances | None |
| [`huggingface`](huggingface.md) | Hugging Face models, datasets and Spaces by repository ID | None |

### Apps, products and jobs

| Source | Reads | Credentials |
| --- | --- | --- |
| [`app_store`](app_store.md) | App Store apps by ID, from the iTunes Lookup API | None |
| [`fdroid`](fdroid.md) | F-Droid apps by package, from its package API and page | None |
| [`flathub`](flathub.md) | Flathub apps by app ID, from its AppStream API | None |
| [`google_play`](google_play.md) | Page hook: a Google Play app's name, icon, developer and category from its page | None |
| [`greenhouse`](greenhouse.md) | Greenhouse job board postings by board and job ID | None |
| [`open_food_facts`](open_food_facts.md) | Products on the Open Food Facts databases by barcode | None |
| [`shopify`](shopify.md) | Page hook: a Shopify store's product page, from the store's own product record | None |
| [`snap_store`](snap_store.md) | Snap Store apps by snap name, from the snapcraft.io info API | None |

### Law, government and companies

| Source | Reads | Credentials |
| --- | --- | --- |
| [`courtlistener`](courtlistener.md) | US court opinions by cluster ID | Token; off without one |
| [`ecfr`](ecfr.md) | Sections and parts of the Code of Federal Regulations | None |
| [`eur_lex`](eur_lex.md) | EU law by CELEX number, ELI or Official Journal ID | None |
| [`federal_register`](federal_register.md) | Federal Register documents by document number | None |
| [`fedlex`](fedlex.md) | Swiss federal law in the classified compilation, by ELI | None |
| [`gleif`](gleif.md) | Legal Entity Identifier records | None |
| [`govinfo`](govinfo.md) | US public laws, bills, US Code sections and the Statutes at Large | None |
| [`legislation_gov_uk`](legislation_gov_uk.md) | UK Acts, statutory instruments and retained EU law | None |
| [`sec`](sec.md) | SEC EDGAR filings, their documents and companies | None |
| [`swiss_parliament`](swiss_parliament.md) | Swiss Parliament business items (Curia Vista) by affair ID | None |
| [`uk_parliament_bills`](uk_parliament_bills.md) | UK Parliament bills and Acts by bill ID | None |
| [`zefix`](zefix.md) | Swiss companies in the commercial register, by register ID | None |

### Science and research

| Source | Reads | Credentials |
| --- | --- | --- |
| [`clinicaltrials`](clinicaltrials.md) | ClinicalTrials.gov clinical studies by NCT number | None |
| [`figshare`](figshare.md) | Figshare articles (datasets, figures, papers) by article ID | None |
| [`gbif`](gbif.md) | GBIF species and occurrence records by key | None |
| [`inaturalist`](inaturalist.md) | iNaturalist observations and taxa by ID | None |
| [`pdb`](pdb.md) | Protein Data Bank structures by PDB ID | None |
| [`pubchem`](pubchem.md) | PubChem compounds by compound ID | None |
| [`ror`](ror.md) | Research organizations by ROR ID | None |
| [`uniprot`](uniprot.md) | UniProtKB proteins by accession | None |
| [`usgs`](usgs.md) | USGS earthquakes and other seismic events by event ID | None |
| [`zenodo`](zenodo.md) | Zenodo records (datasets, software, papers) by record ID | None |

### Museums

| Source | Reads | Credentials |
| --- | --- | --- |
| [`artic`](artic.md) | Art Institute of Chicago artworks by ID | None |
| [`cleveland`](cleveland.md) | Cleveland Museum of Art artworks by accession number | None |
| [`met`](met.md) | The Metropolitan Museum of Art's works by object ID | None |

### Maps

| Source | Reads | Credentials |
| --- | --- | --- |
| [`apple_maps`](apple_maps.md) | Apple Maps links that name a place or a point, from the link itself | None |
| [`google_maps`](google_maps.md) | Google Maps links that name a place or a point, from the link itself (a short link's redirect is its one request) | None |

### Bitcoin

| Source | Reads | Credentials |
| --- | --- | --- |
| [`mempool`](mempool.md) | Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, from mempool.space (Esplora as fallback) | None |

## Catalogues

A catalogue is imported in bulk, one card per item, from the files its publisher offers; nothing
of it is fetched link by link ([catalogues](../operations.md#catalogues)).

| Catalogue | Imports | Credentials |
| --- | --- | --- |
| [`musicbrainz`](musicbrainz_catalogue.md) | Every artist, release group and recording of MusicBrainz's JSON dumps | None |
| [`openalex`](openalex_catalogue.md) | Works cited more than 50 times, from OpenAlex's snapshot | None |
| [`osm`](osm_catalogue.md) | OpenStreetMap's named places and useful points, from the planet file or a region's extract | None |
| [`podcastindex`](podcastindex_catalogue.md) | Every live podcast feed, from Podcast Index's weekly database | None |
| [`software`](software_catalogue.md) | Every crate on crates.io, app on F-Droid and application on Flathub, from their catalogue files | None |
| [`tranco`](tranco_catalogue.md) | The homepages of Tranco's top million sites that answer with a page | None |
| [`wikidata`](wikidata_catalogue.md) | Every item with a Wikipedia article, from the weekly JSON dump, kept current from the recent changes | None |

## Also on

The also-on sources run in this order. Each reads a resource after it is saved and adds keys,
facts and derived links. A name that is also an official source has its also-on part on that
source's page.

| Source | Adds | Credentials |
| --- | --- | --- |
| [`service`](service.md) | The ISRC a music service's own track page states | None |
| [`tidal`](tidal.md) | TIDAL's own barcodes and ISRCs | TIDAL client credentials; off without them |
| [`spotify`](spotify.md) | Spotify's own barcodes and ISRCs | Spotify app credentials; off without them |
| [`musicbrainz`](musicbrainz.md) | What MusicBrainz relates a music link to | None |
| [`musicbrainz_entity`](musicbrainz_entity.md) | A MusicBrainz page as its own hub | None |
| [`musicbrainz_containment`](musicbrainz_containment.md) | An exact recording's releases, as containment facts, never as identity | None |
| [`performers`](performers.md) | An event's performers, keyed to the artist on the page's own service | None |
| [`wikidata`](wikidata.md) | The Wikidata item of a music link | None |
| [`discogs`](discogs.md#also-on) | A Discogs object's key and a release's barcodes | Optional Discogs token or consumer key and secret |
| [`deezer`](deezer.md) | Deezer's own ISRCs and barcodes, and another service's track or album on Deezer by its ISRC or barcode | None |
| [`spotify_isrc`](spotify_isrc.md) | Another service's track or album on Spotify, by its ISRC or barcode | Spotify app credentials; off without them |
| [`tidal_isrc`](tidal_isrc.md) | Another service's track or album on TIDAL, by its ISRC or barcode | TIDAL client credentials; off without them |
| [`itunes`](itunes.md) | Apple Music albums by barcode (a maybe link), and an Apple Music track's preview | None |
| [`itunes_song`](itunes_song.md) | Apple Music's song for another service's track, by its album's barcode, track position and title (a maybe link) | None |
| [`youtube_match`](youtube_match.md) | A label's YouTube upload on Deezer and TIDAL, by artist, title and length (a maybe link) | None |
| [`wikipedia`](wikipedia.md) | The other language editions of a Wikipedia article | None |
| [`wikidata_item`](wikidata_item.md) | A Wikidata item link as its own hub | None |
| [`books`](books.md) | Open Library, Amazon, Apple Books and Wikidata for a book | None |
| [`films`](films.md) | The IMDb title of a film or series | None |
| [`tmdb`](tmdb.md) | TMDB's poster and overview for a film or series card | TMDB credential; off without it |
| [`podcasts`](podcasts.md) | Apple Podcasts, Podcast Index and Wikidata for a show | None |
| [`product_keys`](product_keys.md) | A product's own barcode as its key | None |
| [`own_ids`](own_ids.md) | An official record's own DOI or Wikidata item as its key | None |
| [`open_food_facts`](open_food_facts.md#also-on) | A product's barcode on Open Food, Beauty, Products and Pet Food Facts | None |
| [`stack_exchange`](stack_exchange.md#also-on) | A question's ID, and the original of a duplicate | None |
| [`media_records`](media_records.md#also-on) | `titles`: the title of a media page the page hook could not name in time | None |
| [`federal_register`](federal_register.md#also-on) | A Federal Register document's GovInfo, regulations.gov and public-inspection copies | None |
| [`mempool`](mempool.md#also-on) | A Bitcoin block, transaction or address, or a Lightning node or channel, on the other block explorers | None |
| [`places`](places.md) | Family `osm_wikidata`, `wikidata_osm`, `wikidata_places`, `btcmap`: OpenStreetMap objects and Wikidata items that name each other, and a place's map and review pages | None |
| [`papers`](papers.md) | Family `unpaywall`, `ncbi`, `datacite`, `huggingface`: a paper's open copies, PubMed records and preprint | None |
| [`packages`](packages.md) | Family `crates`, `npm`, `pypi`, `software_heritage`: a package's registry pages, provenance and archive | None |
| [`games`](games.md) | Family `steam`, `wikidata_stores`: a game's other stores | None |
| [`apps`](apps.md) | Family `app_stores`: an app's other stores, bridged by Wikidata | None |
| [`archive`](archive.md) | Family `wayback`: an archived copy of a gone page | None |
| [`editions`](editions.md) | Which release each link of a recording or an album is on (runs last) | None |

A source that records keys declares their forms (`Source::key_forms`, with an example each). Every
form can be searched as recorded (`GET /v1/search?key=`); a test checks this for every registered
source.

## Switches

Each official source has its own switch, on by default:

```toml
[sources.<name>]
enabled = false
```

or `PINAKES__SOURCES__<NAME>__ENABLED=false`. With a source off, its web links get ordinary web
cards, and a link in its own identifier scheme (such as `fr:…`) waits until the source is on
again. `[cards.official] enabled = false` turns off the whole official route.

An also-on source is switched the same way under `[also_on.sources.<id>]`. Turning it off stops
calls to its API and hides the rows it stored; links to the same site that other sources found, or
that people tagged, stay visible.

After an upgrade that adds a source, run `pinakesd reextract web official`, so links that got
plain web cards before are read by their source.

## Politeness

- Every request carries the operator's contact, when set, in the User-Agent (`http.contact`, see
  [operations](../operations.md#deployment-and-egress)). While it is empty, the links of a source
  whose service requires an email address wait and are retried hourly.
- Requests share the per-host budget, `limits.host_requests_per_second` (one per second by
  default), and there is never more than one request in flight per host.
- A service that publishes a lower limit lowers that host's rate; a windowed limit counts as its
  average rate. `limits.host_rates` overrides both.
- A host whose next request is further away than a fetch may wait is retried later.

Each source page names its service's limit.

## Music lookups

### Lookups by ISRC or barcode

Several music sources ask their service's catalogue by an ISRC or a barcode the card already
carries as a key. Each source's page says whether it is one of them. They cover the same cards:

- another service's track, or a video (a video counts as its recording), by its `isrc:` keys;
- another service's album, or a YouTube album playlist, by its `upc:` keys;
- a MusicBrainz recording's or release's page, by the keys its hub gave.

They never look up their own service's links (each reads its own object's record instead), an
artist, another playlist or any other page. A source that asks for albums only reads album cards.

### An ISRC on several recordings

MusicBrainz sometimes gives one ISRC to several recordings. What happens next depends on whether
those recordings agree.

**When they agree, they are one recording.** Recordings agree when all of these hold:

- they have the same artists (by MBID);
- they have the same title once the tags that keep a recording's identity are dropped (a remaster
  or version tag goes; a live, remix or edit tag tells recordings apart);
- they have the same disambiguation (MusicBrainz keeps a live recording's title bare and says
  "live, …" there);
- they are all audio or all video;
- every two known lengths are within 3 seconds.

Then the ISRC is the track's key, and so is the MBID that stands for them all: the smallest MBID,
as `mb:recording:…`. A group holds one recording MBID, so the others are facts. A link
MusicBrainz relates to any of the recordings is keyed by that MBID too, and the MusicBrainz source
takes the union of their links and facts. Another ISRC the recordings carry is a key only when it
names the same recording on its own.

The set is every recording that the recordings' ISRCs reach, and it must agree as a whole: lengths
that drift 3 seconds at each step of a chain are not one recording. The walk reads MusicBrainz's own
records of the exact ISRCs, at most 8 recordings, 16 ISRC lookups and 32 recording reads, all
cached. Past those limits, when MusicBrainz cannot serve a member, or when the set does not agree,
the rule below applies.

**When they disagree, the track's duration decides.** The ISRC alone names none of them. The one
recording whose length is within 3 seconds of the track's, while every other recording's length is
known and is not, is the track's: its MBID is the key and the ISRC a fact. When no recording or
more than one is that close, or the page states no duration, the ISRC is a fact only.

Only MusicBrainz's own data for the exact ISRC decides, never a title search. Every path that keys
a recording goes through one function, `music_sources::recording_key`, so every music source that
reads ISRCs follows the same rule.

Sources that still key different recordings for one resource truly disagree: both keys stay, the
resource joins no recording group, and the run is counted in
`pinakes_also_on_recording_key_conflicts_total`. A MusicBrainz merge, or a new duplicate with a
smaller MBID, changes the recording that stands for a set; rows heal at each resource's next run
(within 30 days, or at once with `pinakesd reextract also-on`).

## Removing a source

Every source (official, also-on or a lookup family) is removed by deleting its own files, its
registry lines and the tests that need it, nothing else. `scripts/test` and `scripts/e2e --mock` then pass as they are. Delete:

- its module: `crates/pinakes-core/src/sources/<name>.rs` (an official source, with its also-on
  half when it has one), `crates/pinakes-core/src/also_on/<id>.rs` or
  `crates/pinakes-core/src/lookups/<family>.rs` (a family, with its also-on sources);
- its lines in each registry it is in: `pub mod <name>;` with its entry in `sources::REGISTRY`,
  `also_on::registry()` (also for an official source's also-on half) or `lookups::FAMILIES`;
- its page `docs/sources/<name>.md` and its rows in the tables on this page;
- its `[sources.<name>]` block, `[also_on.sources.<id>]` lines and own section (`[<name>]`) in
  `pinakes.example.toml`, and its banner paragraph and variables in `.env.example` if it has a
  secret;
- its fixture directories, those named after it:
  `crates/pinakes-core/tests/fixtures/sources/<name>/`,
  `crates/pinakes-core/tests/fixtures/media/<name>/` or
  `crates/pinakes-core/tests/fixtures/lookups/<family>/` (recordings several modules read are in
  `shared/` there, never in a directory named after a source), and its own test file
  (`crates/pinakes-core/tests/<name>.rs` or `tests/also_on_<name>.rs`) if it has one;
- its live end-to-end samples, `e2e/samples/<name>.json`;
- its spelling vectors and identifier pages, the lines of `data/spellings.json` whose `rule` is
  `source/<name>`, the lines of their roots that nothing else names, and the pages without one
  (`null`) in its schemes ([Spelling vectors](../api.md#spelling-vectors),
  [Identifier pages](../api.md#identifier-pages));
  Discogs's stay, with rule `music/discogs` and basis `service`, since `music.rs` roots those
  links the same way;
- the tests that need it. A test asks for the sources it needs first
  (`if !needs_also_on(&["deezer"]) { return; }`, from `pinakes_core::test_support`, with
  `needs_sources`, `needs_families` and `needs_attributes` for the other registries). A name
  the build does not register fails the test, so a misspelled or renamed source never skips one
  silently. A case of a test's loop asks `needs_*` with its name too, and is skipped only during
  a removal. A part of a test that runs only with a source (an extra check) asks `has_sources`
  or `has_also_on`, and a configuration helper sets a source's switch only after
  `switch_also_on`, which fails on a misspelled name; such parts go with the source too. A test
  (`guards_name_registered_sources`) checks the names of every call written as a literal list
  against the registries; a test that returns early during a removal says so in
  `PINAKES_REMOVING_LOG`.

An instance that runs a build without the source may keep a `[sources.<name>]` block (or
`PINAKES__SOURCES__<NAME>__ENABLED`) that only sets `enabled`: the daemon logs a warning and
ignores it. A block that still carries a `token` or a `key` stops the daemon at startup
(`unknown source [sources.<name>]`), as a misspelled name with one does. Remove it, and its
`PINAKES__SOURCES__<NAME>__TOKEN` or `__KEY` variable, with the source.

`scripts/check-removals [--tests] [NAME…]` proves a removal. On a scratch worktree it removes each
source by this list and checks that nothing of the source's own is left. It looks for the
source's ID and display name where every source has lines of its own, parses the example
configuration, `.env.example` and the spelling vectors, and finds a link to its page anywhere,
independently of what it deleted; prose elsewhere that names the source is listed for a reader.
It then builds every test target with warnings as errors, and with `--tests` runs the workspace's
tests too, which catch what the build cannot (a test of another source that relied on it, a
recording it took along). Those tests run with `PINAKES_REMOVING=<name>`: a guard whose names the
removal took returns early instead of failing, as its test goes with the source, and the tests
whose guard names the source are listed. A removal that would need any other edit is named.

Shared code stays: `sources/kit.rs`, `sources/social.rs`, `sources/biodiversity.rs`, `music.rs`,
`music_sources.rs`, `wikipedia.rs`, `lookups/mod.rs`, `lookups/wikidata.rs`, `also_on/media.rs`,
`also_on/client_credentials.rs`, and the clients several sources share in `also_on/*_api.rs`. Code
only one source uses is in that source's files, so a removal leaves no unused code behind. The
exceptions stay as shared code: test helpers for any source, a hook of the `Source` trait that one
source implements today (`apply_title`), and helpers in `sources/mod.rs` and `sources/kit.rs` with
one caller today (`kit::unix_ms`). Shared tables and the stored document's fields stay as they are,
also when the removed source was the only one to fill them: a card type in
`data/local-types.json` or `kit::DATED` (the Bitcoin types), a related entry's `amount_sat`.

A source that needs to know whether another one runs asks by its ID (`also_on::runs_by_id`,
`also_on::enabled_by_id`), never by its module. The keys a removed source recorded stay until they expire (up to their lifetime)
and keep their single-object rule: the daemon writes every registered namespace's rule to the
database at start (`single_object_namespace`), so grouping reads it after the code is gone.
