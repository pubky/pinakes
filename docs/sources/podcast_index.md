# Podcast Index

Pinakes reads shows and episodes on Podcast Index from the Podcast Index API, by feed ID and episode
ID. The API needs the operator's key and secret. Without them, Podcast Index links get ordinary web
cards.

| | |
| --- | --- |
| Source | `podcast_index`, official source |
| Links | `podcastindex.org/podcast/{feed id}`, with `?episode={episode id}` for an episode |
| Fetches | `GET https://api.podcastindex.org/api/1.0/podcasts/byfeedid?id={feed id}` and the show's feed, or `GET …/episodes/byid?id={episode id}` |
| Credentials | Required: `[podcast_index] key` and `secret`, or `PINAKES__PODCAST_INDEX__KEY` and `PINAKES__PODCAST_INDEX__SECRET` ([API keys](../api-keys.md)). Off without both |
| Rate | Shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `PodcastSeries`, `PodcastEpisode` |
| Keys | None from this source; the also-on source `podcasts` keys its cards |
| Credit | None |
| Switch | `[sources.podcast_index] enabled`, on by default |

## Links

| Link | Object |
| --- | --- |
| `https://podcastindex.org/podcast/{feed id}` | A show |
| `https://podcastindex.org/podcast/{feed id}?episode={episode id}` | An episode |

These are the roots. Every other spelling, with `www.`, over `http` or with other parameters, is an
[alias](../api.md#aliases) of its root. An ID is a decimal number without a leading zero. Other
pages of the site, such as its search, are not read here.

## What it fetches

The website is never fetched.

### The API

One request per read, with signed headers (`X-Auth-Key`, `X-Auth-Date`, `Authorization`). The key
and secret go only to `api.podcastindex.org` and never appear in logs, errors or cards.

A feed or episode ID that the API does not know is `missing`, and so is an episode of another show
than the page names. An answer for another ID is an error, and the read is retried.

When Podcast Index refuses the pair (a 401, or a 403 whose `description` names the key), the page
gets a web card. The source then leaves the pair unused for an hour, during which its pages get web
cards. Each refusal is counted in
`pinakes_source_token_refusals_total{source="podcast_index"}`, which the `ProviderKeyRefused` alert
reads. The also-on `podcasts` lookups use the same pair and count a refusal by the same rule. A
refusal there does not pause this source, nor this source's refusal those lookups.

### The show's feed

For a show, the source also reads the feed its record names, for the show's
[episodes](../api.md#episodes). The feed is read up to 8 MiB and cut there, and cached for the
card's lifetime. A feed that does not answer within three seconds leaves the show's stored list as
it is.

## Card

`site` is "Podcast Index".

| Field | Show (`PodcastSeries`) | Episode (`PodcastEpisode`) |
| --- | --- | --- |
| `name` | The title | The title |
| `description` | The description, as plain text | The description, as plain text |
| `image` | The artwork | The episode's image, else its show's |
| `stream` | | The enclosure, when it is a public link ([stream](../api.md#stream)) |
| `related` | | The show's Podcast Index page (`rel: "show"`), by the show's title |
| `episodes` | The latest 5 that have a `stream`; `GET /v1/resources/{id}/episodes` lists more | |

| Fact | Meaning |
| --- | --- |
| `author` | A show's author |
| `website` | The show's or episode's website |
| `language` | A show's language |
| `feed_url` | A show's feed, as Podcast Index reads it |
| `published` | When an episode was published |
| `duration` | An episode's length in seconds |
| `show` | An episode's show page on Podcast Index |
| `page_url` | The Podcast Index page |

## Keys and groups

This source records no keys. The also-on source `podcasts` finds the show or episode on
Apple Podcasts, in its feed, on Spotify and on other services, and records the keys that join them in
a [group](../api.md#groups).

## Turn it off

```toml
[sources.podcast_index]
enabled = false
```

Podcast Index links then get ordinary web cards. The `podcasts` also-on source still asks
Podcast Index with the same pair.

## Examples

- `https://podcastindex.org/podcast/48166` (Acquired, `PodcastSeries`)
- `https://podcastindex.org/podcast/255500?episode=61684996715` (an episode, `PodcastEpisode`)
