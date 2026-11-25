# `tmdb`: TMDB posters and overviews for films and series

This source runs on film and series cards that have no image or no description of their own. It
looks the title up on [TMDB](https://www.themoviedb.org) by its exact IDs, never by title, and
fills in TMDB's poster and overview. It is off without the operator's TMDB credential.

| | |
| --- | --- |
| Source | `tmdb`, also-on source |
| Runs on | Resolved `Movie` and `TVSeries` cards with an IMDb ID or a Wikidata item, and no image or no description of their own; never `themoviedb.org` pages |
| Asks | TMDB: `GET https://api.themoviedb.org/3/find/{tt}?external_source=imdb_id` and `GET …/find/{Q-id}?external_source=wikidata_id` |
| Credentials | Required: `TMDB_API_KEY`, or `[tmdb] api_key` ([API keys](../api-keys.md#tmdb)). Off without it |
| Rate | At most 4 requests per second on `api.themoviedb.org`; the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | None |
| Links | None |
| Facts | `tmdb_title`, `tmdb_poster`, `tmdb_overview`, `tmdb_overview_cut` |
| Switch | `[tmdb] enabled` and `[also_on.sources.tmdb] enabled`, both on by default |

## What it does

1. **The IDs.** The source takes the card's IMDb title ID, the `imdb:{tt…}` key that
   `films` records, and its Wikidata item, a `wikidata:{Q-id}` key or the card's own
   item link. A card with several IDs of one kind gets nothing.
2. **The answer.** An answer names a title only when it holds exactly one movie or series
   (`movie_results` and `tv_results` together). When the IMDb ID and the item name different
   titles, the card gets nothing.
3. **The type.** A `Movie` card takes only a movie, and a `TVSeries` card only a series.
4. **The card.** What TMDB says is recorded as facts, and the source asks for the card to be read
   again at once. The card takes its values at that read.

An image or description that TMDB gave earlier counts as none of the card's own. A card that has
its own of both asks nothing.

### What the card takes

| Field | Value | When |
| --- | --- | --- |
| `image` | `https://image.tmdb.org/t/p/w500{poster}`, TMDB's 500-pixel poster | The card has no image of its own (a page's own image and a Commons image win) |
| `description` | The English overview | The card has no description of its own |

The poster is a link: Pinakes never downloads it.

## Keys, links and facts

The source records no keys and no links, so it joins nothing.

| Fact | Meaning |
| --- | --- |
| `tmdb_title` | `movie/{id}` or `tv/{id}` |
| `tmdb_poster` | The poster's path |
| `tmdb_overview` | The overview, cut to 512 bytes at a sentence or a word |
| `tmdb_overview_cut` | `true` when the overview was cut |

### How long TMDB's values stay

- A card takes TMDB's values only from live rows. Rows are renewed at each run that TMDB answers,
  and expire 60 days after the last one.
- A card drops expired values at its next read. For cards that are not read again (final,
  dead-lettered or orphaned), the hourly maintenance removes them
  ([operations](../operations.md#also-on)).
- Change lines keep a card's earlier values for `limits.change_retention_days` (90 by default).
  While the credential is set, startup refuses more than 120. A change line that holds a TMDB
  value is deleted after 120 days in any case, whether the source is on or off.

## Cost

One TMDB request per ID of a card, at most two. The source runs again after each save that changes
the card, and every 30 days. Answers are cached for `limits.discovery_cache_secs` (an hour by
default).

To fill cards that were stored before you set the credential, run `pinakesd reextract also-on` with
the daemon stopped.

### The credential

`TMDB_API_KEY` takes either of two forms:

- an API Read Access Token (recommended), sent as `Authorization: Bearer`;
- a v3 API key (32 hexadecimal digits), added to the request's query on the wire only.

A value of another form stops startup, and the message never repeats it. Neither form appears in a
log, an error, the request cache or a stored row.

### Failures

| Answer | What happens |
| --- | --- |
| 401 | The run fails and is retried. It is logged as `TMDB refused the credential` and counted in `pinakes_also_on_service_errors_total{service="tmdb",class="refused"}` and `pinakes_source_token_refusals_total{source="tmdb"}`, which the `ProviderKeyRefused` alert reads |
| 404 | TMDB has no such title |
| 429 or 5xx | The run is retried later |

A failed run keeps the rows TMDB gave before until they expire.

## Credit

Each value TMDB gave has its [credit](../api.md#credits): `provider` is `tmdb` and `source_url` is
the title's page (`https://www.themoviedb.org/movie/{id}` or `/tv/{id}`), with no author and no
licence. The overview's credit has `lang: "en"`, and `excerpted: true` when it was cut.

`/v1/info` lists TMDB in `attributions` while the source is on, and after that for as long as a
stored card still holds a TMDB value. The entry gives the notice "This product uses the TMDB API but
is not endorsed or certified by TMDB.", the URL of TMDB's logo and `https://www.themoviedb.org`. A
client that shows a value credited to `tmdb` shows the notice and the logo, with the logo less
prominent than its own marks.

## Turn it off

```toml
[tmdb]
enabled = false
```

`[also_on.sources.tmdb] enabled = false` does the same, and so does an unset credential. No request
is made (the egress refuses `api.themoviedb.org`), TMDB's stored rows are hidden from the next
start, and every card that holds its values is due at once and loses them at that read.
