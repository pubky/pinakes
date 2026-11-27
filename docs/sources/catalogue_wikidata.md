# The Wikidata catalogue

The Wikidata catalogue holds one [catalogue card](../operations.md#catalogues) for each Wikidata
item that has a Wikipedia article, imported from Wikidata's weekly JSON dump and kept current from
Wikidata's recent changes. A link someone shares for such an item (its Wikipedia article in one of
its 16 biggest editions, its IMDb, TMDB, Google Maps or Steam page, …) gets its card from the
catalogue without being fetched ([shared links](../operations.md#shared-links-a-catalogue-holds)). No key is needed.

| | |
| --- | --- |
| Catalogue | `wikidata`, a large catalogue (one large full import at a time) |
| Reads | The dumps directory `https://dumps.wikimedia.org/wikidatawiki/entities/` and its newest dump `<date>/wikidata-<date>-all.json.bz2` (about 104 GB) with its `wikidata-<date>-sha1sums.txt`; Wikidata's API `https://www.wikidata.org/w/api.php` (`list=recentchanges`, `wbgetentities`) |
| Credentials | None. The operator's contact (`http.contact`) is required: without it the daemon does not start the importer (`catalogue status` shows it `waiting` for `http.contact`, and `pinakes_catalogue_needs_contact` is 1) and `catalogue import wikidata` refuses |
| Rate | The dump on two connections (Wikimedia's limit); the API one request at a time and at least 0.3 s apart, with `maxlag=5`, waiting as long as a `Retry-After` or a `maxlag` refusal asks (up to 5 minutes; longer, or `maxlag` refusals past the tries, fails the run with `rate_limited`, and nothing more is asked of that host until the time it asked for has passed, a day at most). A dump read (a download round or a stream) that brings under 8 MB in ten minutes of waiting for the server fails `download` and is taken up again from its cursor. In the daemon each API request also takes the egress's turn for `www.wikidata.org`, so the importer's and the resolver's requests to it go one at a time, at the egress's 200 a minute for the host. HTTPS only, no redirects |
| Cards | About 21.4 million at full size (items with a Wikipedia article, without Wikimedia's own pages) |
| Updates | Every 15 minutes from the recent changes, at once while more than an hour behind; a daemon down for over 5 days waits for the next weekly dump |
| Card types | Schema.org types by the item's class, else `Place` (with a point) or `Thing` ([type](#type)) |
| Keys | `wikidata:Q…`, MusicBrainz, OpenStreetMap, `imdb:`, `isbn:`, `doi:`, and `url:` keys of the item's pages ([keys](#keys-and-links)) |
| Credit | Wikidata (CC0) |
| Switch | `[catalogues.wikidata] enabled`, on by default; `limit` for a part |

## What it imports

An item becomes a card when it has an article in at least one Wikipedia edition and is not one of
Wikimedia's own pages: an item whose class (`P31`) is a Wikimedia category, disambiguation page,
template, list, module, project page, redirect or another of the 20 internal classes gets none.
Every edition counts: a site `<code>wiki` that is not one of Wikimedia's other wikis (Commons,
Wikispecies, Meta, Wikidata, …). Properties, lexemes and items without an article are skipped.

| Field | From the item |
| --- | --- |
| `uri` | The English Wikipedia article; without one, the article of the first edition in Pinakes' order of editions (`de`, `fr`, `es`, `it`, `ja`, `ru`, `pt`, `zh`, …), else of the edition whose site name sorts first. Spelled as a shared Wikipedia link is (`https://<lang>.wikipedia.org/wiki/<Title>`) |
| `name` | The label in English, else `mul`, else German, French, Spanish, …, else any label, else the article's title |
| `other_names` | English aliases, then labels in other languages, at most 15 |
| `description` | The description in English, else `mul`, German, French or Spanish |
| `type` | See [type](#type) |
| `labels` | The English labels of its genres (`P136`), then of its classes (`P31`, without "human"), as tag labels; at most 16 |
| `keys` | See [keys](#keys-and-links) |
| `related_uris` | Its official websites (`P856`), at most 3 |
| `point` | Its coordinate on Earth (`P625`), the first at the best rank |
| `fame` | The number of Wikipedia editions with an article, on a log scale: `ln(1 + editions) / ln(301)`, at most 1 |

Only best-rank statements count: preferred ones if the property has any, else normal ones, never
deprecated ones.

### Type

The item's first class (`P31`) found in a fixed table gives the type: `Person` (human), `Movie`
(films, also animated, short, silent and television films), `TVSeries` (also miniseries, animated
and web series, television programs), `TVSeason`, `TVEpisode` (also television specials), `MusicAlbum`, `MusicRecording`, `MusicComposition`, `MusicGroup`, `Book`,
`BookSeries`, `VideoGame`, `Game`, `Taxon`, `ScholarlyArticle`, organisations, schools and
universities, newspapers and periodicals, `WebSite`, `SoftwareApplication`, languages, events and
festivals, countries, states, cities and administrative areas, mountains, waters, museums,
churches and other buildings, parks, stations and airports, and a few more (`entity.rs`, `TYPES`).
An item of no class in the table is a music artist, album or recording by its MusicBrainz and
Discogs identifiers (as Pinakes reads Wikidata items everywhere, `music_enrichment::ItemKind`),
else a `Place` when it has a coordinate, else a `Thing`.

### Labels

A genre or a class is an item too, so its label is in the dump, often after the cards that name
it. The import keeps the English (else `mul`) label of every class and genre it reads (an item
with a `P279` "subclass of", or of a genre class) in `labels.tsv` in its directory. A card whose
label items were not read yet is listed in `pending.tsv`; when the dump is read, the labels still
unknown are asked of the API (`wbgetentities`, labels only, 50 items a request, at most 50,000
items), and those cards are written again with all their labels. A label longer than 20
characters, or that makes no tag label, is left out. Updates read the labels they don't know from
the API the same way, and add them to `labels.tsv`. In a full import of a catalogue that holds
cards already, a waiting card is written on its first pass with only the labels read so far, so
until that last step (hours at full size) search's tag filters and suggested tags miss its other
labels. Those rewrites count once toward the cards the import read, which the catalogue's shrink
floor compares with the cards it holds.

## Keys and links

A key comes from a property only when the item has exactly one valid best-rank value for it:
two values mean the item spans two things, and a key that names two objects would join their
groups.

| Key | From |
| --- | --- |
| `wikidata:Q…` | The item |
| `mb:artist:`, `mb:release-group:`, `mb:recording:` | The MusicBrainz ID at the item's own level (an artist's item keys its artist, an album's its release group, a recording's its recording; an item that carries two levels, or only a release, keys none) |
| `osm:relation/…`, `osm:way/…`, `osm:node/…` | `P402`, `P10689`, `P11693` |
| `imdb:tt…` | `P345`, for a title (a person's IMDb page is a `url:` key instead) |
| `isbn:` (ISBN-13) | `P212` or `P957`, when both name one book |
| `doi:` | `P356`, in lower case |
| `url:<resource id>` | The item's Wikipedia articles (the 16 biggest editions first; a link to a smaller edition's article is fetched as any shared link is), and its pages on other services: Google Maps (`P3749`), Apple Maps, Tripadvisor, Yelp, app stores (App Store, Google Play, F-Droid, Flathub, Snap Store, Microsoft Store, Homebrew), game stores (Steam, GOG, Epic, itch.io, IGDB, MobyGames, PCGamingWiki, HowLongToBeat), for a film or series (by its type, or by an IMDb title or TMDB id whatever its class) IMDb, TMDB, Letterboxd, Trakt, Netflix and the others of the [`films`](films.md) table, for a book the [`books`](books.md) table, for a podcast the podcasts table (a book or podcast with an IMDb title or TMDB id gets both its own table and the films table), and for music its service pages at its level (Spotify, Apple Music, Deezer, Tidal, …). Each page also adds the keys it names by itself (an F-Droid page `android:<package>`) |

A shared link that is one of these pages, or names one of these keys, gets the card. Official
websites are not keys: they join the item's group as related links, but a homepage isn't always
the thing itself.

## Updates

The importer reads the recent changes of items (namespace 0: edits, new items, deletions,
merges) after the stored position (a time), up to a minute ago and at most 6 hours of them per
run, 500 a page, then asks for the changed items' sitelinks, 50 per `wbgetentities` request, one
request at a time, and reads in full (labels, claims, sitelinks; 50 a request) only those that have
a Wikipedia article or whose label the catalogue keeps (a class or genre): most changed items have
neither. An item changed several times in a run is fetched once. An answer over the cap is asked
again in halves; an item whose answer alone is over it keeps its card until it changes again
(`pinakes_catalogue_items_skipped_total`). Items an answer leaves out are asked again one by one;
one still left out fails the run (`api`), as does an answer without its list of changes. A changed item is written again only
when its card changed. An item that is gone (deleted), merged into another (a redirect), no longer
has an article, became a Wikimedia page, or now makes a card the table refuses leaves the
catalogue; a new item with an article joins it. A changed class or genre gives its new label to the
cards read after it (`labels.tsv`); cards already written keep their labels until they change.
Each run moves the position to the end of the changes it read, and the cards are current as of
that time (`current_as_of`). While the position is more than an hour behind, the next run starts at
once; else every 15 minutes.

After an import the position is the dump's date, which is two or three days old by the time the
file is out, so the first runs catch up. When the position an update left is more than five days
old (the daemon was down), the changes are not caught up one by one: the update asks for a full
import once a dump newer than the imported one is out, and waits for it until then (the update lag
shows meanwhile). An import's position older than 25 days (beyond the recent changes Wikidata
keeps) is handled the same way.

While the updates catch up after an import, `pinakes_catalogue_catching_up{catalogue="wikidata"}`
is 1 and `CatalogueUpdateLag` holds back (up to five days past the allowed two), since the
dump's data was already days old ([allowed update lag](../operations.md#allowed-update-lag)).

## Import

A first import (or one an update asked for):

1. finds the newest dated directory that lists the full JSON dump (`<date>/`);
2. downloads it to `catalogues.dir/wikidata` on two connections, each its half of the file, in
   rounds of 512 MB whose progress is saved: a cut-off download resumes where each connection
   stopped (`If-Range`, so a file replaced meanwhile fails the run with `dump_changed`);
3. checks its SHA-1 against `wikidata-<date>-sha1sums.txt` (`checksum` when it differs);
4. reads it on half the machine's cores: the bzip2 blocks are found and decoded in parallel, the
   lines parsed in parallel, and the cards written in dump order, 1,000 per transaction; the read
   resumes at the block of the last batch written;
5. writes the cards that wait for labels again ([labels](#labels)), deletes the dump, and removes
   the cards of an earlier import that this one didn't read.

A read that gives no card is refused (`empty`), and one with more than 1% of lines (and over 100)
that are not JSON or too large fails (`bad_dump`), without removing any card. A failure that makes
the files useless (`checksum`, `dump_changed`, `dump_gone`, `dump_missing`, `label_files`, a
`bz2_…` block that doesn't decode, `bad_dump`) deletes the work files, and the next try starts over
from the newest dump (a local `--from` file is never deleted: the import fails until the catalogue is dropped and the
fixed file imported); the third such failure of one dump refuses it (`refused`): the run is
cleared, the large-import turn is free, and the import waits for a newer dump
([a refused dump or file](../operations.md#a-refused-dump-or-file)). A first import (or an update
handing over to one) that finds the refused dump still the newest ends at once. A whole import that read under half the cards the
catalogue holds meets the catalogue's shrink floor: it fails with `shrunk`, removes no card and is
tried again, until `pinakesd catalogue allow-shrink wikidata` lets it remove the rest
([a full import that shrinks the catalogue](../operations.md#a-full-import-that-shrinks-the-catalogue)).
Any other failure resumes from the cursor. What to do about each: [a Wikidata import that keeps failing](../operations.md#a-wikidata-import-that-keeps-failing). The read logs its progress
about once a minute (`wikidata dump download`, `wikidata dump read so far`, `wikidata changes read
so far`).

The cards are current as of the dump's date, and the updates read the changes from that date on.
An entity line larger than 32 MB is skipped and counted (`oversized`), as is a line that is not
JSON (`bad`); a bzip2 block that doesn't decode fails the run (`bz2_block`), as does a file cut
short (`bz2_truncated`). The import's summary line in the log (`wikidata dump read`) gives the
lines read, the cards kept, the items without an article, Wikimedia's pages and the lines skipped.

### A part of Wikidata

`[catalogues.wikidata] limit = N` imports the first N items with an article in dump order (the
oldest, mostly well-known ones). The dump is then read as a stream over HTTP, from the start, and
the read stops at the limit: nothing is downloaded to disk, and a few hundred megabytes are read
for tens of thousands of cards. An update of a limited catalogue changes and removes its cards, and
adds new items only while it holds fewer than the limit. Taking the limit out of the
configuration lifts it at the restart: the updates add the new items they see, and the next full
import, also one an update asks for after a long gap, reads the whole dump (the 104 GB download).
An import under way keeps the limit it started with. A limit given on the command line
(`catalogue import wikidata --limit N`) stays until a full import without one: `catalogue import
wikidata --request`, or `catalogue import wikidata` with the daemon's Wikidata off (unless an
update has asked for an import already, after a long gap: that import keeps the limit too).

## Commands

```sh
pinakesd --config <path> catalogue import wikidata [--from FILE|URL] [--limit N] [--force]
pinakesd --config <path> catalogue import wikidata --update [--since TIME]
pinakesd --config <path> catalogue import wikidata --request
```

`catalogue import wikidata` runs a full import now, through the daemon's code
([commands](../operations.md#status-and-drop)): the catalogue's lock, the large-import turn, the disk
check, the cursor; a full import cut off earlier is taken up, and it prints the cards before and
after. `--from` reads a local `.json.bz2` file or one on the web instead of the newest dump
(`https://` only, without credentials or a query: the URL is kept in the import's cursor until it ends; a streamed file has no checksum, the blocks' own CRCs check it). `--limit` sets
the limit for this run, and is kept for the updates after it and for the imports they ask for,
not for one asked for with `--request` (a limit in the configuration is not kept: taking it out
lifts it); a limit below the cards stored
needs `--force`. A dump's date comes from its file name, else (for `latest-all.json.bz2`) from a
week before the file was written, and the updates read the changes from then on. `--from` is
refused while an import of another file is under way.

`--update` runs an update now instead, after a finished full import (it reads the recent changes
with the catalogue's limit: `--from` and `--limit` are refused with it; a configured limit below
the cards stored needs `--force`, as the update may hand over to a full import with it). `--since` (RFC 3339) moves its
position first: an update cut off earlier is set aside, and a full import an update asked for is no
longer asked for; refused for a time to come, before a first full import has finished, while a
full import is under way and while one asked for with `--request` waits. A `--since` more than
five days back is held by the gap rule like any update's
position: it waits for a newer dump, then a full import is asked for (a download, or a stream when
the catalogue has a limit). Each refuses while a daemon runs the importer, and a failure is recorded
and resumes as the task's would. `--request` asks the daemon for a full import instead.

`scripts/catalogue-sample-wikidata` checks the importer on a sample of the real dump: it imports
the first records into a new database, counts the cards, keys and labels, checks that
`/v1/search` finds a card and that a shared Wikipedia link gets its card, runs an update from the
real recent changes, and checks that switching the catalogue off hides it; then it drops the
database. It needs `PINAKESD`, `DATABASE_URL` (a server where it may create a database) and
`PINAKES__HTTP__CONTACT`.

## Credit

Wikidata's data is CC0; a card names Wikidata as its catalogue (`from`), and a card built from
it takes its Wikipedia article's summary with Wikipedia's credit
([extras](../operations.md#shared-links-a-catalogue-holds)).

## Turn it off

`[catalogues.wikidata] enabled = false` and a restart: the importer stops, the cards leave search
at once, links built from them go back to fetching, and the rows stay until
`pinakesd catalogue drop wikidata` ([turn a catalogue off](../operations.md#turn-a-catalogue-off)).
