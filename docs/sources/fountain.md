# Fountain

Shows and episodes on Fountain. Fountain has no public API, so this source is a page hook: the
web route reads the page as usual, and the page's own metadata names the card (its title less
" • Listen on Fountain", its description and cover; Fountain's site-wide Organization JSON-LD never
does). The hook adds what makes it the show's or episode's feed card. No key, no account.

## Pages

`https://fountain.fm/show/{id}` and `https://fountain.fm/episode/{id}`. The app data a page
embeds in its scripts names:
- the show's RSS feed (`rss`), taken only when the data names exactly one;
- for an episode, the item GUID of the record whose `_id` is the page's own and whose `_type`
  is `EPISODE` (the show's own record, also on the page, is not the item).

A page whose data names no feed, or an episode page without its own record, keeps its page card.
Nothing behind a login is read.

## What it reads

Nothing beyond the page the web route fetched.

## Card

The page's card, typed `PodcastSeries` (a show) or `PodcastEpisode` (an episode), with its feed
as `podcast_feed` and an episode's item GUID as `podcast_guid`: the facts a podcast feed's own
card carries. The podcasts also-on source then reads the feed (and the item), and finds the show
or episode on Apple Podcasts, Podcast Index and the services Wikidata names. `details.from` is
`fountain`.

| Fact | Meaning |
| --- | --- |
| `podcast_feed` | The show's RSS feed |
| `podcast_guid` | An episode's item GUID in that feed |

## Examples

- `https://fountain.fm/show/V71wG3DhMtF9fDqaJRWX`
- `https://fountain.fm/episode/Yj4aqLaHXcVmxgXbMLHb`
