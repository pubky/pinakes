# Substack

Substack publications, posts and profiles, from their pages' Open Graph tags and JSON-LD. No
key and no account. A publication's card names its RSS feed; a link to the feed itself
(`https://{name}.substack.com/feed`) is the [feed](feed.md) source's.

## Links

A publication is named by its subdomain, a post by its publication and slug, a profile by its
handle: `https://{name}.substack.com/`, `https://{name}.substack.com/p/{slug}` and
`https://substack.com/@{handle}` are the roots. Share links
(`https://open.substack.com/pub/{name}/p/{slug}`), `www.substack.com` and a capitalised
subdomain are aliases of them. A publication on its own domain is read as an ordinary web page.

## What it reads

- A publication: `GET https://{name}.substack.com/`, its Open Graph tags and feed link. A 404
  is `missing`.
- A post: `GET https://{name}.substack.com/p/{slug}`, its JSON-LD article. A 404 is `missing`.
- A profile: `GET https://substack.com/@{handle}`, its JSON-LD person. A handle that does not
  exist sends the fetch to Substack's search: `missing`.

A request Substack refuses (a challenge, 401 or 403) gives the card of the link's shape alone
with the `blocked` fact: the link still resolves, groups and counts.

## Card

A publication is a `Blog` named by its title, with its description and image. A post keeps the
article type its page declares (`NewsArticle`, else `BlogPosting`), named by its title, with its
summary, image and date. A profile is a `Person` named by its display name, with its bio and
avatar.

| Fact | Meaning |
| --- | --- |
| `publication` | A post's publication (its subdomain) |
| `published` | A post's date, RFC 3339 |
| `handle` | A publication's subdomain or a profile's handle |
| `feed` | A publication's RSS feed |
| `blocked` | `true` when Substack refused the request |

## Examples

- `https://on.substack.com/p/model-behavior-rick-rubin`
- `https://on.substack.com/` (a publication)
- `https://substack.com/@substack` (a profile)
