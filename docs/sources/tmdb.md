# `tmdb`: TMDB posters and overviews for films and series

An [also on](../api.md#also-on) source, only with the operator's TMDB credential (`TMDB_API_KEY`,
[operations](#operations)).

`tmdb`: a resolved `Movie` or `TVSeries` card with no image or no description of its own (one
TMDB gave counts as none) is looked up on [TMDB](https://www.themoviedb.org) by its exact IDs,
never by title:
- its IMDb title ID, the `imdb:{tt…}` key [`films`](films.md) records (an IMDb, Netflix, Trakt or
  other film link), with `GET https://api.themoviedb.org/3/find/{tt}?external_source=imdb_id`;
- its Wikidata item, a `wikidata:{Q…}` key or the card's own item link (a Wikidata item's card),
  with `external_source=wikidata_id`.

An answer names a title only when it holds exactly one movie or one series (`movie_results` and
`tv_results` together), of the card's own type: a `Movie` card takes a movie, a `TVSeries` card a
series. Several IDs of one kind, an answer with none or several, or an IMDb ID and an item that
name different titles give nothing. A card with its own image and description asks nothing,
and a themoviedb.org card, TMDB's own page, is never looked up: it keeps what its page gives.

What TMDB says is recorded as facts, `tmdb_title` (`movie/{id}` or `tv/{id}`), `tmdb_poster` (the
poster's path) and `tmdb_overview` (cut at 512 bytes, at a sentence or a word, with
`tmdb_overview_cut`), and the card takes them at its next read, written into the card itself:
- `image`: `https://image.tmdb.org/t/p/w500{poster}` (TMDB's 500-pixel poster, near the width of
  the Commons thumbnails cards show), only when the card has no image of its own: a page's own
  image or a Commons image wins;
- `description`: the overview (English), only when the card has none;
- each with its [credit](../api.md#credits): provider `tmdb`, `source_url` the title's page on
  TMDB (`https://www.themoviedb.org/movie/{id}` or `/tv/{id}`), no author or licence.

The poster is a link on the card: Pinakes never downloads it.

## Attribution

A client that shows TMDB's data or images shows TMDB's logo, less prominent than its own marks,
and a notice that it uses the TMDB API but is not endorsed or certified by TMDB. While the source
runs, and after while a stored card still holds a TMDB value, `/v1/info` lists both in
`attributions` (provider `tmdb`: the notice "This product uses the TMDB API but is not endorsed
or certified by TMDB.", the logo's URL and `https://www.themoviedb.org`); a client that shows a
value credited to `tmdb` shows them.

## Operations

`tmdb` costs one TMDB request per ID of a card (at most two: its IMDb ID and its item), cached an
hour, again every 30 days and after each save that changes the card. TMDB's API is paced at
the default host pace (`limits.host_requests_per_second`, 1/s), and at no more than 4 requests/s
when that is raised (TMDB's upper limit "sits somewhere in the 40 requests per second range").

It runs only with `TMDB_API_KEY` set ([api-keys](../api-keys.md#tmdb)): an API Read Access Token,
sent as `Authorization: Bearer` (recommended), or a v3 API key, added to the request's query on
the wire alone (never in a log, an error, the egress cache or a stored row). `[tmdb] enabled =
false` or `[also_on.sources.tmdb] enabled = false` switches it off as an unset key does: no
request (the egress refuses `api.themoviedb.org`), TMDB's stored rows hidden from the next start,
and every card that holds them due at once, losing TMDB's values at that read. A refused
credential (401) is logged (`TMDB refused the credential`) and counted in
`pinakes_also_on_service_errors_total{service="tmdb",class="refused"}`; a 429 or 5xx is retried
later. Either keeps the rows TMDB gave before until they expire.

TMDB lets its data be cached for six months at most. Its rows live 60 days from the last read
that answered; a card takes TMDB's values only from live rows, so it drops them at its first read
after they expire, and the hourly maintenance drops them from cards that are not read again
(final, dead-lettered or orphaned cards, [operations](../operations.md#also-on)); change lines
keep a card's earlier values for `limits.change_retention_days` (90), startup refuses more than
120 while the credential is set, and a change line holding a TMDB value goes after 120 days in
any case. To fill stored
cards after the key is set, run `pinakesd reextract also-on` with the daemon stopped.
