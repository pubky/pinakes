# Media records

Pinakes names the cards of film, series, book and podcast pages after the object they show. Such
pages often name their site ("The Movie Database"), the title in another language ("Ka hopita"), the
site after the title ("Darknet Diaries | Podcastindex.org") or nothing at all (an app shell,
"Netflix"). This source is a page hook and a wall hook: it claims no links, the web route reads the
page as usual, and the hook takes the name from the object's exact record. No key is needed.

| | |
| --- | --- |
| Source | `media_records`, official source, page hook and wall hook, with an also-on half ([`titles`](#also-on)) |
| Links | Pages on TMDB, Trakt, Netflix, Goodreads, Letterboxd, IMDb, Open Library, TVmaze and Podcast Index, and walled pages on IMDb, Pocket Casts, Castro and Overcast, read by the web route |
| Fetches | Wikidata (`https://www.wikidata.org/w/api.php`), `GET https://openlibrary.org/{works or books}/{id}.json`, `GET https://api.tvmaze.com/shows/{id}`, `GET https://itunes.apple.com/lookup?id={id}&entity=podcast&country=us` and the show's feed |
| Credentials | None |
| Rate | At most about 20 requests a minute on `itunes.apple.com`, 200 a minute on `www.wikidata.org`, 3 per second on `openlibrary.org` and 2 per second on `api.tvmaze.com` (the services' published limits); the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower. Every source that asks these hosts shares the rate |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | `Movie`, `TVSeries`; `CreativeWork` and `PodcastSeries` for walled pages; else the page's or link's type |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.media_records] enabled`, on by default |

A record is exact: the Wikidata item that carries the page's own ID, or the service's own record
under that ID. Nothing is searched by name. A failed or refused request, or an ID that no record
carries, leaves the page's own card.

## Links

### Pages the hook names

| Page | Record |
| --- | --- |
| TMDB `themoviedb.org/movie/{id}`, `/tv/{id}` | The Wikidata item with that TMDB ID (P4947, P4983) |
| Trakt `trakt.tv/movies/{slug}`, `/shows/{slug}` | The item with that Trakt ID (P8013) |
| Netflix `netflix.com/title/{id}`, also under a region | The item with that Netflix ID (P1874) |
| Goodreads `goodreads.com/work/editions/{id}` | The item with that Goodreads work ID (P8383) |
| Letterboxd `letterboxd.com/film/{slug}` | The item with that Letterboxd ID (P6127) |
| IMDb `imdb.com/title/{tt…}`, also on `m.imdb.com` | The item with that IMDb ID (P345) |
| Open Library `openlibrary.org/works/{OL…W}`, `/books/{OL…M}` | Open Library's own record |
| TVmaze `tvmaze.com/shows/{id}` | TVmaze's own record |
| Podcast Index `podcastindex.org/podcast/{id}` | The page's own title, without the site's name |

Only the object's own page qualifies, with or without the title in its address, never a cast,
season, review or episode page under it. Podcast Index pages reach the hook only as web pages, when
the `podcast_index` source does not read them.

### Walled pages

When a page answers with an [access wall](../design.md#access-walls), the wall hook makes these
cards:

| Page | Record |
| --- | --- |
| IMDb `imdb.com/title/{tt…}`, also on `m.imdb.com` | The item with that IMDb ID (P345) |
| Pocket Casts `pca.st/itunes/{id}`, Castro `castro.fm/itunes/{id}`, Overcast `overcast.fm/itunes{id}` with or without a slug | iTunes' record of the show. Overcast sends a reader to its sign-in page, which counts as a wall |

## What it fetches

- **Wikidata:** a search by the ID, the read of each hit to compare the ID exactly, and one request
  for the item's English label and description. An ID that two items carry is ambiguous and names
  nothing.
- **Open Library and TVmaze:** one request each. The record must be the one asked. A work that Open
  Library merged into another names nothing.
- **iTunes:** one request, in the `us` storefront. It must answer exactly one show with that ID.
- **A walled show's feed:** the feed iTunes names (`feedUrl`), read up to 8 MiB and cut there, for
  the show's [episodes](../api.md#episodes). A feed that is not read and parsed within three seconds
  and the hook's remaining time leaves the show's stored list as it is.
- **Podcast Index:** nothing.

All requests run within the web fetch's deadline, at most 3 seconds for a page hook and 6 for a wall
hook. Answers are cached and shared with the also-on sources. Wikidata's label is kept for a day.

When Wikidata does not answer in time, the page keeps its own card at first, and the also-on half
[`titles`](#also-on) names it later.

## Card

The name is the record's: the item's English label, Open Library's title, or TVmaze's or iTunes'
name. The other fields depend on the page:

| Page | Description | Image | Type |
| --- | --- | --- | --- |
| TMDB, Goodreads, IMDb | The item's replaces the page's | The page's | By the item's IDs (below) |
| Trakt, Netflix (app shells) | The item's replaces the page's | None: the service's own image goes | By the item's IDs |
| Letterboxd | The page's | The page's | By the item's IDs |
| Open Library | The record's, when it has one | The page's | Unchanged |
| TVmaze | The record's, when it has one | The record's, when it has one | `TVSeries` |
| Podcast Index | The page's | The page's | Unchanged |
| Walled IMDb title | The item's | None | By the item's IDs, else `CreativeWork` |
| Walled podcast player | None | iTunes' artwork | `PodcastSeries` |

A description that replaces the page's is empty when the item has none. Open Library's and TVmaze's
are plain text, cut to 300 characters.

"By the item's IDs" means `Movie` when the item has only a movie's IDs, `TVSeries` when it has only
a series', and otherwise no change. The type a link itself gives (`films`, `books`)
comes first: a TMDB movie link is a `Movie` and a Goodreads or Open Library link a `Book`.

`from` is `media_records`, except on a Podcast Index page, which keeps the page's.

| Fact | Meaning |
| --- | --- |
| `wikidata` | The Q-id of the item that named the card |

## Keys and groups

The Q-id is a fact only, and the source records no keys. The also-on sources `films`, `books` and
`podcasts` key these pages.

## Also on

`titles` is this source's fallback. It runs on every resolved page whose record is a Wikidata item:
TMDB, Trakt, Netflix, Goodreads, Letterboxd and IMDb. It reads the same item without the hook's time
limit and records two facts:

| Fact | Meaning |
| --- | --- |
| `title` | The item's English label |
| `title_description` | Its English description |

When the hook did not name the card and the card bears another name, the source asks for the link
to be read again at once. The finished card then takes the title by the same rule as the hook: the
description and image change as in the table above. A card the hook named keeps the hook's values.

Each page costs the item's search and read, usually cached already, and one request for its label.
The rows live 30 days and expire after twice that. A failed request fails the run, which is retried
while the stored rows stay.

## Credit

None. TVmaze states CC BY-SA for its data.

## Turn it off

```toml
[sources.media_records]
enabled = false

[also_on.sources.titles]
enabled = false
```

With the first off, pages keep the card their own metadata makes, and a walled page gets no card
from this source. `titles` then finds nothing either, and its stored facts go at its next run. With
only the second off, pages that Wikidata was too slow for keep their own name, and the stored facts
stop showing at the next start.

## Examples

- `https://www.themoviedb.org/movie/603`
- `https://www.themoviedb.org/tv/1396`
- `https://trakt.tv/shows/breaking-bad`
- `https://www.netflix.com/title/70143836`
- `https://openlibrary.org/works/OL27482W`
- `https://www.goodreads.com/work/editions/1540236`
- `https://www.tvmaze.com/shows/169/breaking-bad`
- `https://podcastindex.org/podcast/577105`
