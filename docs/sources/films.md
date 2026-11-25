# `films`: the IMDb title of a film or series

This source runs on links to films and series. It finds the title's IMDb ID by exact IDs, never by
title, and records it as a key together with the title's pages on other services as
[also on](../api.md#also-on) links. No key is needed.

| | |
| --- | --- |
| Source | `films`, also-on source |
| Runs on | A title's own page on IMDb, TMDB, Letterboxd, TVmaze, Netflix, Trakt, Rotten Tomatoes, TheTVDB, Apple TV, MUBI, MyAnimeList or AniList ([links](#links-it-reads)) |
| Asks | Wikidata (`https://www.wikidata.org/w/api.php`); TVmaze (`GET https://api.tvmaze.com/shows/{id}`, `/lookup/shows?imdb={tt}`); Letterboxd (`GET https://letterboxd.com/imdb/{tt}/`) |
| Credentials | None |
| Rate | At most 200 requests a minute on `www.wikidata.org` and 2 per second on `api.tvmaze.com` (the services' published limits); the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower. `letterboxd.com`: the shared default |
| Rows live | 30 days; rows expire after 60 |
| Keys | `imdb:{tt…}` |
| Links | The title's pages on the services above, exact |
| Facts | `wikidata`, `title_type` |
| Switch | `[also_on.sources.films] enabled`, on by default |

## What it does

### Links it reads

| Service | Link | Wikidata property |
| --- | --- | --- |
| IMDb | `imdb.com/title/{tt and 7 or 8 digits}` (also on `m.imdb.com`) | P345 |
| TMDB | `themoviedb.org/movie/{id}`, `themoviedb.org/tv/{id}`, with or without the title after the ID | P4947, P4983 |
| Letterboxd | `letterboxd.com/film/{name}` | P6127 |
| TVmaze | `tvmaze.com/shows/{id}`, with or without the title after it | P8600 |
| Netflix | `netflix.com/title/{id}`, `netflix.com/{region}/title/{id}` | P1874 |
| Trakt | `trakt.tv/movies/{name}`, `trakt.tv/shows/{name}` | P8013 |
| Rotten Tomatoes | `rottentomatoes.com/m/{name}`, `rottentomatoes.com/tv/{name}` | P1258 |
| TheTVDB | `thetvdb.com/dereferrer/movie/{id}`, `thetvdb.com/dereferrer/series/{id}` | P12196, P4835 |
| Apple TV | `tv.apple.com/movie/{umc.cmc.…}`, `tv.apple.com/show/{umc.cmc.…}` | P9586, P9751 |
| MUBI | `mubi.com/films/{id}` | P7299 |
| MyAnimeList | `myanimelist.net/anime/{id}` | P4086 |
| AniList | `anilist.co/anime/{id}` | P8729 |

Each host is read with or without `www.`, and with or without a trailing slash. From Trakt
down, only the address shown is read, because it is the one that holds the ID Wikidata records.

Only the title's own page is looked up. A page under it names the title but is not it, and gets
nothing: IMDb's `fullcredits` or reviews, a TMDB season or episode, TVmaze's episode list.

### Finding the title

The source first finds the IMDb ID:

- an IMDb link names it;
- TVmaze's record of the show names it;
- any other link reaches it through the one Wikidata item that carries the link's ID, when that
  item has exactly one IMDb ID.

For the Wikidata search, the index only proposes: each hit is read back and its value compared
exactly. An ID that two items carry is ambiguous and gives nothing. The title's item is the one
that carries its IMDb ID, or the one the link's ID led to.

### The title's other pages

The item gives one page per service, in the order of the table above. A property counts only when
the item has exactly one value for it, and a valid one. Letterboxd and TVmaze are asked by IMDb ID
only when the item gives no page there:

- Letterboxd redirects `imdb/{tt}/` to its film. An unknown ID answers a page instead of a
  redirect, which counts as none.
- TVmaze redirects the lookup to its show, whose record must name the same IMDb ID.

A title's card never lists its own service. A title whose item has no IMDb ID still gets the
item's pages, but no key, and neither Letterboxd nor TVmaze is asked.

## Keys, links and facts

The key is `imdb:{tt…}`. It names one title, so a group holds at most one. Every link of a title
with an IMDb ID carries that key, so a tagged Netflix, Trakt or Letterboxd link joins the title's
[group](../api.md#groups). The links of a title without one carry no key.

All links are exact. `via` is `wikidata` for a page from the item, `letterboxd` or `tvmaze` for one
those services named.

| Fact | Meaning |
| --- | --- |
| `wikidata` | The Q-id of the title's item. A fact only: the IMDb ID is the join |
| `title_type` | `Movie` or `TVSeries`, when the item's IDs are a movie's or a series' only |

`title_type` reads TMDB's, TheTVDB's and Apple TV's movie IDs and Trakt's film ID for a movie, and
TMDB's, TheTVDB's, Apple TV's and TVmaze's series IDs for a series.

### Card types

A resolved card takes its type from its link:

| Link | Type |
| --- | --- |
| TMDB movie, Letterboxd film, Trakt movie | `Movie` |
| TMDB series, TVmaze show, Trakt show | `TVSeries` |
| Netflix title | `title_type`, from the card's next read; else the type `media_records` gave it; else `CreativeWork` |
| Any other link | The type its page gives |

## Cost

Each title costs two Wikidata requests (the search and the read of its hits). A TVmaze show first
costs its TVmaze record. Letterboxd and TVmaze cost one lookup each when the item gives no page
there. Answers are cached for `limits.discovery_cache_secs` (an hour by default).

When a request fails after the IMDb ID is known, what was found stands, nothing recorded before is
deleted, and the run comes back for the rest. When the request that finds the IMDb ID fails, the
run fails and is retried while its stored rows stay. A refusal by Letterboxd, such as a challenge
page, counts as no film.

## Credit

Links that TVmaze named say `via: "tvmaze"`. TVmaze states CC BY-SA for its data.

## Turn it off

```toml
[also_on.sources.films]
enabled = false
```

Its stored keys, links and facts stop showing at the next start, and its groups re-form without
them. The types that come from the link alone stay. A type a Netflix card took from `title_type`
goes at its next read, and `tmdb` gets no IMDb ID from these links.
