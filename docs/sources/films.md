# `films`: the IMDb title of a film or series

An [also on](../api.md#also-on) source.

`films`: an IMDb title (`imdb.com/title/tt…`), a TMDB movie or series, a
Letterboxd film, a TVmaze show, a Netflix title, or a title's page on any other service the item
lists below (Rotten Tomatoes `rottentomatoes.com/m/{name}` or `/tv/{name}`, Trakt, Apple TV, MUBI,
MyAnimeList, AniList, TheTVDB) is looked up exactly, never by title. Only the title's own page
is: a page under it (IMDb's `fullcredits` or reviews, a TMDB season or episode, TVmaze's episode
list) names the title but is not it, and gets nothing.
- The IMDb title ID is the join, as the key `imdb:{tt…}`. An IMDb link names it, TVmaze's record
  of a show names it, and any other link reaches it through the one Wikidata item with that
  link's ID (`haswbstatement:P1258=m/1009113-halloween`, read back and compared exactly; an ID
  two items carry is ambiguous and gives nothing). A page of another service is recognized by
  the address the item's own link to it has (the property's URL with the value in place), less
  its trailing slash.
- The title's Wikidata item (by its IMDb ID) gives its IMDb, TMDB, Letterboxd, Trakt, Rotten
  Tomatoes, TheTVDB, Apple TV, Netflix, MUBI, TVmaze, MyAnimeList and AniList pages, its Q-id as
  the `wikidata` fact, and `title_type` (`Movie` or `TVSeries`) when its IDs are a movie's or a
  series' only.
- Letterboxd and TVmaze are asked by IMDb ID only when the item has no link there: Letterboxd's
  `imdb/{tt}/` redirects to its film (an unknown ID answers a page, not a redirect), and
  TVmaze's `lookup/shows?imdb=` to its show, whose record must name the same IMDb ID. TVmaze's
  data is CC BY-SA; its links say `via: "tvmaze"`.
- Every link of a title with an IMDb ID carries `imdb:{tt…}`, so a tagged Netflix, Trakt or
  Letterboxd link joins the title.

A title never lists its own service. A Trakt link is typed (a `Movie` or a `TVSeries`) by its
address; a page found through the item keeps the card its page makes, and joins by the title's
IMDb ID.

## Operations

Each film or series costs two Wikidata requests (a TVmaze show first its TVmaze record), and a
Letterboxd and a TVmaze request when the item has no link there. TVmaze is paced at 2 requests/s
(it allows at least 20 calls every 10 seconds).
