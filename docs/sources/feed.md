# Feeds (for walled pages)

Cards for links whose page answers with an access wall: a bot challenge, 401 or 403 (staging had
20 such links on theblock.co). This source is a wall hook: it claims no links by their shape. When
the web route meets a wall, the same site's own feeds are searched for an item whose link is
exactly the page's address, and that item makes the card. No key.

Feeds list recent items only, so a link is found while its article is in the feed, typically a
day or two; older links stay unreachable. A card once found is kept when the item later leaves
the feed: a wall never confirms that a page is gone.

## Where it looks

On the page's own origin, in order, stopping at the first match:

1. the feeds `/feed`, `/rss.xml`, `/feed.xml`, `/atom.xml` and `/index.xml` (RSS 2.0 or Atom);
2. the Google News sitemaps that `/robots.txt` names on the same host (`Sitemap:` lines whose
   address says `news`), at most two.

Matching is exact: the item's link and the page's address must be the same once Pinakes
normalizes them; no search by title, no near spelling. Every feed,
sitemap and `robots.txt` goes through the shared discovery cache (an hour; `robots.txt` a day;
a failed fetch five minutes) and the host's normal rate, within the web fetch's deadline and at
most 6 seconds. A host whose feeds all answered and none listed any item is remembered for a day.

## Card

`Article` (from an RSS or Atom feed) or `NewsArticle` (from a news sitemap), named by the item's
title and described by its description (at most 300 characters), with its image. `details.from`
is `feed`.

| Fact | Meaning |
| --- | --- |
| `author` | The item's authors (`dc:creator`, Atom `author`) |
| `published` | Its date |
| `feed` | The feed or sitemap it came from |

## Examples

- `https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058`
