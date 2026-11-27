# Feeds (for walled pages)

Pinakes makes cards for links whose page answers with an access wall (a bot challenge, 401 or
403) from the same site's own feeds. This source is a wall hook: it claims no links by their shape.
When the web route meets a wall, Pinakes searches the site's RSS and Atom feeds and Google News
sitemaps for an item whose link is exactly the page's address, and that item makes the card. No
key is needed.

| | |
| --- | --- |
| Source | `feed`, official source, wall hook |
| Links | None claimed: any web page that answers with an access wall |
| Fetches | `GET {origin}/feed`, `/rss.xml`, `/feed.xml`, `/atom.xml`, `/index.xml`; `GET {origin}/robots.txt`; up to two news sitemaps it names |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`), per site |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | `Article` (from a feed), `NewsArticle` (from a news sitemap) |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.feed] enabled`, on by default |

## Pages

The hook runs when the web route fetches a page and meets an access wall. It looks only on the
page's own origin.

Feeds list recent items only, so a link is found while its article is in the feed, typically for a
day or two. Older links stay unreachable. A card once found is kept when the item later leaves the
feed, because a wall never confirms that a page is gone
([access walls](../design.md#access-walls)).

Matching is exact: the item's link and the page's address must be the same once Pinakes
normalizes them. There is no search by title and no near spelling: `{address}?page=2` or
`{address}-2` is another page.

## What it fetches

On the page's own origin, in this order, stopping at the first match:

1. The feeds `/feed`, `/rss.xml`, `/feed.xml`, `/atom.xml` and `/index.xml` (RSS 2.0 or Atom).
2. `/robots.txt`, and the Google News sitemaps it names on the same origin: `Sitemap:` lines whose
   path says `news`, at most two.

Every feed, sitemap and `robots.txt` goes through the shared discovery cache. A feed or sitemap is
kept an hour, `robots.txt` a day, and a failed fetch five minutes. Each request also takes the
host's normal pace. The whole search runs within the web fetch's deadline and takes at most 6
seconds. At the default pace of one request a second, a site's first walled link may spend that
time on the page and its feeds. The news sitemaps are then read on a later attempt, once the feeds
are cached.

A feed or sitemap over 1 MiB is read up to its last whole item within that limit. At most 5,000
items of one feed or sitemap are searched.

A host whose feeds all answered and none listed any item, and whose `robots.txt` did not fail, is
remembered for a day: its walled links ask no more during that time. Nothing is concluded about a
host when any of these happens, and it is asked again next time:

- a feed or `robots.txt` answered a rate limit, a timeout or a server error;
- a request failed;
- a feed was cut before its first whole item.

## Card

The card is an `Article` when the item came from an RSS or Atom feed, and a `NewsArticle` when it
came from a news sitemap. An item without a title makes no card.

- `name` is the item's title, as plain text.
- `description` is the item's description (RSS) or summary, else content (Atom), as plain text of
  at most 300 characters. A news sitemap item has none.
- `image` is the item's image, when it is an `https` URL.
- `from` is `feed`.

An RSS title, an Atom text of `type="text"` (the default) and a news sitemap title are plain text:
what looks like markup in them stays as written (`Using <title> in HTML`). An RSS description and
an Atom text of `type="html"` or `type="xhtml"` are HTML: their tags are taken out, an inline tag
splitting no word (`H<sub>2</sub>O` is `H2O`). Both are read on one line.

An RSS item's image is the first of its child elements that is one of these:

- a `media:thumbnail`, unless its `medium` names something other than `image`;
- a `media:content` that is an image by its `medium`, or, without a `medium`, by its `type` (no
  `type` counts as an image; a video or a sound does not);
- an `enclosure` whose `type` is an image.

An Atom entry's image is its `media:thumbnail`. A news sitemap item's image is its `image:loc`.

| Fact | Meaning |
| --- | --- |
| `author` | The item's authors (RSS `dc:creator`, Atom `author`) |
| `published` | The item's date (RSS `pubDate`, Atom `published` or `updated`, the sitemap's publication date) in RFC 3339, read as podcast feeds' dates are: RFC 2822 or RFC 3339 with the item's own offset (`+0200` stays `+02:00`), a zone name RFC 2822 does not list counting as `UTC` for `UTC` and as no offset for any other (`CEST`) and stored in UTC, and only within years 1 to 9999 |
| `feed` | The address of the feed or sitemap the item came from |

## Keys and groups

A wall hook's card has no keys and joins no [groups](../api.md#groups).

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.feed]
enabled = false
```

Walled pages then keep the wall's outcome on the web route, unless another wall hook answers for
them.

## Examples

- `https://www.theblock.co/news/business/2026-10-19-mesh-visa-partner-spend-crypto-stablecoins-421058`
