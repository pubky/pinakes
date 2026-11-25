# WordPress

Pinakes improves the cards of posts on any site running WordPress, from the site's own REST
record of the post. This source is a page hook: it never claims links by their shape. The web
route fetches the page as usual. When the page names its own REST record, the card takes the
post's editorial title, excerpt, dates and featured image from it. A site's editorial title often
differs from the shorter social title its Open Graph tags carry. No key is needed.

| | |
| --- | --- |
| Source | `wordpress`, official source, page hook |
| Links | None claimed: web pages that name their own post record (see [Pages](#pages)) |
| Fetches | `GET …/wp-json/wp/v2/posts/{id}?_embed=wp:featuredmedia&_fields=…` (or `?rest_route=/wp/v2/posts/{id}`) on the page's own origin |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`), per site |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | The page's article type (`Article` or a subtype, such as `NewsArticle`), else `BlogPosting` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.wordpress] enabled`, on by default |

## Pages

The hook runs on a page the web route fetched with a 200 answer, an HTML content type and an
ordinary card. It does not run on a card that ActivityPub or a podcast feed made.

A page names its post through any of these, on the page's own origin, in this order:

1. A `Link` header: `<…/wp-json/wp/v2/posts/{id}>; rel="alternate"; type="application/json"`.
2. The same link in the page's head: `<link rel="alternate" type="application/json" href="…">`.
3. The shortlink in the page's head, `<link rel="shortlink" href="…/?p={id}">`. Pinakes then reads
   the record through `?rest_route=/wp/v2/posts/{id}`, which works with and without pretty
   permalinks.

A post ID is a positive decimal number of at most 12 digits. A WordPress installed in a
subdirectory, such as `/blog/wp-json/…`, keeps that directory. The directory is at most 100 bytes of
ASCII letters, digits, `-`, `_` and `/`. A link with any other directory is not read. Pinakes
rebuilds the record's address from the origin, that directory and the ID alone, so no other path or
query the page names is requested.

Not read: records on another origin, pages' records (`/wp/v2/pages/…`) and any other link.

## What it fetches

One request per page, on the page's own origin, for example:

```text
GET https://{site}/wp-json/wp/v2/posts/{id}?_embed=wp:featuredmedia&_fields=id,link,title,excerpt,date_gmt,modified_gmt,_links,_embedded
```

The request runs within the web fetch's deadline and takes at most 3 seconds. A redirect is not
followed. Any answer other than 200, an error or a timeout leaves the page's own card.

The record must be for the same post ID, name the page's own address as its `link`, and have a
title. Otherwise the page keeps its own card.

## Card

The card is the page's own card with these changes:

- `name` is the post's rendered title, as plain text.
- `description` is the post's excerpt as plain text, without WordPress's trailing "[…]", at most
  300 characters.
- `image` is the post's featured image, when it has an `https` one.
- A page typed as an article (`Article` or a subtype, such as `NewsArticle` or `BlogPosting`)
  keeps its type; any other becomes `BlogPosting`.
- `from` is `wordpress`.

| Fact | Meaning |
| --- | --- |
| `published` | When the post was published, UTC, RFC 3339 |
| `modified` | When the post was last changed, UTC, RFC 3339 |
| `wordpress_post` | The post's ID on its site |

## Keys and groups

A page hook's card joins nothing: `wordpress_post` is a fact only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.wordpress]
enabled = false
```

WordPress pages then keep the card the web route makes from their own metadata.

## Examples

- `https://bitcoinmagazine.com/markets/no-uptober-yet-as-bitcoin-slips`
