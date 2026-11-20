# Feeds (for walled pages)

Cards for links whose page answers with an access wall: a bot challenge, 401 or 403. This source is a wall hook: it claims no links by their shape. When
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
most 6 seconds. At one request a second, a site's first walled link may spend that time on
the page and its feeds; the news sitemaps are then read on a later attempt, once the feeds are
cached. A feed or sitemap over 1 MB is read up to its last whole item within that megabyte, and
at most 5,000 items of one are searched. A host whose feeds all answered and none listed any item
is remembered for a day; a rate limit, a timeout, a server error or a feed cut before its first
item says nothing about the host, which is asked again.

## Card

`Article` (from an RSS or Atom feed) or `NewsArticle` (from a news sitemap), named by the item's
title and described by its description (at most 300 characters), with its image. An RSS item's
image is its `media:thumbnail`, an `enclosure` typed as an image, or a `media:content` that is an
image by its `medium` or, without one, by its `type` (no `type` counts as an image; a video or a
sound does not). `from` is `feed`.

| Fact | Meaning |
| --- | --- |
| `author` | The item's authors (`dc:creator`, Atom `author`) |
| `published` | Its date |
| `feed` | The feed or sitemap it came from |

## Examples

- `https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058`
