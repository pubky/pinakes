# WordPress

Posts on any site running WordPress. This source is a page hook: it never claims links by their
shape. The web route fetches the page as usual, and when the page names its own REST record, the
card takes the post's editorial title, excerpt, dates and featured image from it. A site's
editorial title often differs from the shorter social title its OpenGraph tags carry. No key.

## Pages

A page names its post through any of these, on the page's own origin:

- a `Link: <…/wp-json/wp/v2/posts/{id}>; rel="alternate"; type="application/json"` header;
- the same as `<link rel="alternate" type="application/json" href="…">` in its head;
- the shortlink `<link rel="shortlink" href="…/?p={id}">`, read through
  `?rest_route=/wp/v2/posts/{id}`, which works with and without pretty permalinks.

Records on another origin, pages' records (`/wp/v2/pages/…`) and any other link are not read.
The record must name the page's own address (`link`); otherwise the page keeps its own card.

## What it reads

`GET …/wp-json/wp/v2/posts/{id}?_embed=wp:featuredmedia&_fields=id,link,title,excerpt,date_gmt,modified_gmt,_links,_embedded`
(a few KB), within the web fetch's deadline and at most 3 seconds. A failed or refused request
leaves the page's own card.

## Card

The page's card with the post's title, excerpt (at most 300 characters) and featured image. A
page typed as an article keeps its type (`NewsArticle`, `BlogPosting`…); any other becomes
`BlogPosting`. `details.from` is `wordpress`.

| Fact | Meaning |
| --- | --- |
| `published`, `modified` | When the post was published and last changed (UTC) |
| `wordpress_post` | The post's id on its site |

## Examples

- `https://bitcoinmagazine.com/markets/no-uptober-yet-as-bitcoin-slips`
