# Podcast Index

Podcast Index pages, read from Podcast Index's API: a show by its feed id, an episode by its
episode id. The website is never fetched. The API needs the operator's Podcast Index key and
secret (`[podcast_index]`, or `PINAKES__PODCAST_INDEX__KEY` and `__SECRET`), the pair the also-on
`podcasts` source uses. Without both, the source declines and the page gets an ordinary web card.
The pair goes only to `api.podcastindex.org`, signed per request, and never appears in logs,
errors or cards.

## Links

- a show: `https://podcastindex.org/podcast/{feed id}`;
- an episode: `https://podcastindex.org/podcast/{feed id}?episode={episode id}`.

With or without `www.`; each spelling is an alias of the one above. A feed or episode id the API
does not know, or an episode of another show than the page names, is `missing`.

## What it reads

`GET https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id={feed id}` for a show, and
`GET https://api.podcastindex.org/api/1.0/episodes/byid?id={episode id}` for an episode, with the
signed headers (`X-Auth-Key`, `X-Auth-Date`, `Authorization`). For a show, then its feed (the
record's `url`, read on past the page cap to 8 MiB and cut there, cached for the card's lifetime), for the
show's latest episodes ([Episodes](../api.md#episodes)); a feed that does not answer, or not
within three seconds, leaves the show's stored list as it is.

## Card

A show is a `PodcastSeries`, an episode a `PodcastEpisode`; `site` is "Podcast Index". `name` is
the title, `description` its text without markup, `image` the artwork (an episode's own image,
else its show's). An episode's [`stream`](../api.md#stream) is its enclosure (`enclosureUrl`,
`enclosureType`, `duration`), when that is a public link.

| Fact | Meaning |
| --- | --- |
| `author` | A show's author |
| `website` | The show's or episode's website |
| `language` | A show's language |
| `feed_url` | A show's feed, as Podcast Index reads it today |
| `published` | When an episode was published |
| `duration` | An episode's length in seconds |
| `show` | An episode's show page on Podcast Index |
| `page_url` | The Podcast Index page |

An episode's card names its show in `related` (`rel: "show"`, the show's Podcast Index page).

A show's latest episodes, from its feed, are listed by `GET /v1/resources/{id}/episodes`
([Episodes](../api.md#episodes)); the show's card carries the latest 5 that have public audio
or video (a `stream`) in `episodes`. The joins (Apple Podcasts, the feed, Spotify) come from the
also-on [`podcasts`](../api.md#also-on) source.

## Examples

- `https://podcastindex.org/podcast/48166` (Acquired, `PodcastSeries`)
- `https://podcastindex.org/podcast/255500?episode=61684996715` (an episode, `PodcastEpisode`)
