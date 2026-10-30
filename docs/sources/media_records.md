# Media records

Film, series, book and podcast pages whose HTML does not name their object: a page that names
its site ("The Movie Database"), the title in another language ("Ka hopita"), the site after
the title ("Darknet Diaries | Podcastindex.org") or nothing at all (an app shell, "Netflix").
This source is a page hook and a wall hook: it claims no links. The web route fetches the page
as usual, and the card then takes its object's name from the object's exact record. That record
is the item or record that carries the page's own ID, never a search by name. A failed or
refused request, or an ID no record carries, leaves the page's own card.

## Pages

| Page | Record |
| --- | --- |
| TMDB `themoviedb.org/movie/{id}`, `/tv/{id}` | The Wikidata item with that TMDB ID (P4947, P4983) |
| Trakt `trakt.tv/movies/{slug}`, `/shows/{slug}` | The item with that Trakt ID (P8013) |
| Netflix `netflix.com/title/{id}` | The item with that Netflix ID (P1874) |
| Goodreads `goodreads.com/work/editions/{id}` | The item with that Goodreads work ID (P8383) |
| Letterboxd `letterboxd.com/film/{slug}` | The item with that Letterboxd ID (P6127) |
| IMDb `imdb.com/title/tt…` (a page with nothing in it) | The item with that IMDb ID (P345) |
| Open Library `openlibrary.org/works/OL…W`, `/books/OL…M` | Open Library's own record (`{path}.json`) |
| TVmaze `tvmaze.com/shows/{id}` | TVmaze's record (`api.tvmaze.com/shows/{id}`; CC BY-SA) |
| Podcast Index `podcastindex.org/podcast/{id}` | The page's own title, without the site's name |

Only the object's own page qualifies, never a cast, season, review or episode page under it.

Walled pages:
- an IMDb title (`imdb.com/title/tt…`) whose page answers with a challenge gets a card from the
  item with that IMDb ID (P345);
- a Pocket Casts, Castro or Overcast page of an Apple podcast (`pca.st/itunes/{id}`,
  `castro.fm/itunes/{id}`, `overcast.fm/itunes{id}`, which sends a reader to its sign-in page) gets
  one from iTunes' record of the show (`itunes.apple.com/lookup`).

When the item's requests do not answer within the hook's time (Wikidata busy or slow), the page
keeps its own card at first. The also-on source `titles` then reads the same item without a time
limit, records its English label and description as the facts `title` and `title_description`,
and when the card bears another name has the link read again: the finished card takes them by
the same rule as the hook (a card the hook named keeps the hook's).

## What it reads

For a Wikidata page, the hub's exact lookup (a search by the ID, each hit read back and compared),
then `wbgetentities&props=labels|descriptions&languages=en` for the item's English label and
description. Open Library, TVmaze and iTunes are one request each. All of them are read within
the web fetch's deadline, at most 3 seconds for a page hook. Answers are cached, so the also-on
sources reuse them.

## Card

- The name is the record's: the item's English label, Open Library's title, TVmaze's or iTunes'
  name.
- The description:
  - TMDB (whose page answers in the server's language) and Goodreads (an edition list): the
    item's description replaces the page's;
  - Trakt and Netflix (app shells): the item's description replaces the page's, and the service's
    own image goes;
  - Letterboxd: the page's description and image stay;
  - Open Library and TVmaze: their record's description, and TVmaze's image.
- A film or series item types the card `Movie` or `TVSeries` when its IDs are one or the
  other's only.
- `from` is `media_records`.

| Fact | Meaning |
| --- | --- |
| `wikidata` | The Q-id of the item that named the card |

## Examples

- `https://www.themoviedb.org/movie/603`
- `https://www.themoviedb.org/tv/1396`
- `https://trakt.tv/shows/breaking-bad`
- `https://www.netflix.com/title/70143836`
- `https://openlibrary.org/works/OL27482W`
- `https://www.goodreads.com/work/editions/1540236`
- `https://www.tvmaze.com/shows/169/breaking-bad`
- `https://podcastindex.org/podcast/577105`

## Also on

`titles`, this source's fallback: a TMDB, Trakt, Netflix, Goodreads, Letterboxd or IMDb page whose
card the page hook could not name in time gets the label of the Wikidata item that carries the
page's ID as the fact `title` and its description as `title_description`, and when the card
still bears another name, the resource is read again so its finished card takes them. It is off
when this source is. Each such page costs the item's search and read (usually cached already)
and one request for its English label.
