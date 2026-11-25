# Fountain

Pinakes improves the cards of show and episode pages on Fountain. Fountain has no public API, so
this source is a page hook: it claims no links, the web route reads the page as usual, and the hook
adds the show's feed and the episode's item GUID from that page. No key is needed.

| | |
| --- | --- |
| Source | `fountain`, official source, page hook |
| Links | `fountain.fm/show/{id}`, `fountain.fm/episode/{id}`, read by the web route |
| Fetches | Nothing beyond the page the web route fetches |
| Credentials | None |
| Rate | Shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | `PodcastSeries`, `PodcastEpisode` |
| Keys | None (facts only); the also-on source `podcasts` keys its cards |
| Credit | None |
| Switch | `[sources.fountain] enabled`, on by default |

## Links

The hook runs on `https://fountain.fm/show/{id}` and `https://fountain.fm/episode/{id}`, with or
without `www.` and with any parameters, and on a link that redirects to one. An ID is letters and
digits. Pages under a show and profile pages (`/p/{npub}`) are not read here.

## What it fetches

Nothing of its own. From the app data that the page embeds in its scripts the hook reads:

- **The show's RSS feed** (`rss`), only when the data names exactly one.
- **An episode's item GUID**, from the record whose `_id` is the page's own and whose `_type` is
  `EPISODE`. The show's own record, which is also on the page, is not the item.

A page whose data names no feed, and an episode page without its own record, keep their page card.

## Card

The card is the page's, from its own metadata, with these changes:

- the type is `PodcastSeries` for a show and `PodcastEpisode` for an episode;
- an episode names its show in `related` (`rel: "show"`, the feed), unless the feed's address
  carries a credential;
- `from` is `fountain`.

| Fact | Meaning |
| --- | --- |
| `podcast_feed` | The show's RSS feed |
| `podcast_guid` | An episode's item GUID in that feed |

A podcast feed's own card carries the same two facts.

## Keys and groups

The page records no key, because it is no service's own record of the show. The also-on source
`podcasts` reads the feed and the item, finds the show or episode on Apple Podcasts,
Podcast Index and the services Wikidata names, and gives an episode without a stream the item's
[stream](../api.md#stream). It gives the page keys as follows:

- A show's page takes the show's keys only when the feed names it as the show's page (the channel's
  `<link>`). Otherwise the page lists the show's links without joining them.
- An episode's page takes the item key only when the item's `<link>` names it. It takes the
  Apple track and file that iTunes lists for the item's GUID in either case, once iTunes confirms
  the Apple ID Podcast Index names for the feed.

## Turn it off

```toml
[sources.fountain]
enabled = false
```

Fountain pages then keep the card their own metadata makes. The link alone still types them
`PodcastSeries` or `PodcastEpisode`, but they name no feed, so `podcasts` does not find them.

## Examples

- `https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX`
- `https://fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb`
