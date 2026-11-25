# Substack

Pinakes reads Substack publications, posts and profiles from their public pages: the Open Graph
tags and JSON-LD each page carries. No key or account is needed. A profile is social content; a
publication and a post are not.

| | |
| --- | --- |
| Source | `substack`, official source |
| Links | `{name}.substack.com/`, `{name}.substack.com/p/{slug}`, `open.substack.com/pub/{name}/p/{slug}`, `substack.com/@{handle}` |
| Fetches | `GET https://{name}.substack.com/`, `GET https://{name}.substack.com/p/{slug}` or `GET https://substack.com/@{handle}` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)); a profile within the hour (social content); a link Substack refuses, a day later, doubling to every 7 days |
| Card types | `Blog`, `BlogPosting` or the article type the page declares (such as `NewsArticle`), `Person` |
| Keys | None (facts only) |
| Social content | Profiles, and a publication's card while Substack refuses the request (a `Person`): 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.substack] enabled`, on by default |

## Links

A publication is named by its subdomain, a post by its publication and slug, and a profile by its
handle. These are the roots:

| Object | Root |
| --- | --- |
| Publication | `https://{name}.substack.com/` |
| Post | `https://{name}.substack.com/p/{slug}` |
| Profile | `https://substack.com/@{handle}` |

These links are [aliases](../api.md#aliases) of a root:

- share links, `https://open.substack.com/pub/{name}/p/{slug}`, with or without a query;
- `www.substack.com` for a profile;
- a subdomain or handle written with capital letters.

A publication's subdomain and a post's slug are 1 to 63 lower-case letters, digits and hyphens,
not starting or ending with a hyphen. A handle is letters, digits, `_`, `.` and `-`, at most 64
bytes. Substack's own subdomains (`www`, `open`, `support`, `substack`, `on-substack`, `api`) are
never a publication.

Not read here: other pages of a publication, such as `/archive`; `substack.com/home` and other
Substack pages; a publication on its own domain, which is read as an ordinary web page; and the
publication's feed (`https://{name}.substack.com/feed`), which is read as any feed link is.

## What it fetches

Each read makes one request, for the root's page:

- A publication: `GET https://{name}.substack.com/`, for its Open Graph tags and feed link.
- A post: `GET https://{name}.substack.com/p/{slug}`, for its JSON-LD article.
- A profile: `GET https://substack.com/@{handle}`, for its JSON-LD person.

The answers mean:

- A 404 or 410: the card is `missing`.
- A profile fetch that ends on Substack's search (`/search/…`), which is where a handle that does
  not exist leads: the card is `missing`.
- A 401, 403 or 451, or an access wall (a challenge): Substack refused the request (see below).
- A 429: the read is retried after at least the time Substack names (up to an hour), or 5 minutes
  when it names none.
- A 408 or 5xx: the read is retried after at least a minute.
- Any other status: an error, and the read is retried.

When Substack refuses the request, the card is the link's shape alone, marked with the fact
`blocked`. A post's card is then a `SocialMediaPosting` named "Post on substack.com"; a
publication's or a profile's is a `Person` named "Profile on substack.com". Its description is
"Blocked by the site." The link still resolves, groups and counts. On a refresh, a refusal keeps the
card read before, as an access wall on a web page does. After a refusal the link is asked again
after a day, then after 2, 4 and 7 more days, and every 7 days while one refusal follows another
([refusals](../operations.md#failures-and-retries)).

## Card

`site` is `substack.com`. A description is cut to 200 characters.

- A **publication** is a `Blog`, named by its page title without " | Substack" (or
  `{name}.substack.com` without a title). Its description is the page's, without Substack's own
  "Click to read …" ending. Its image is the page's.
- A **post** keeps the article type its JSON-LD declares (such as `NewsArticle`), else it is a
  `BlogPosting`. It is named by its title, with the page's summary, image and date.
- A **profile** is a `Person`, named by its display name, with its bio and avatar when the page
  describes a person. The bio is published without e-mail addresses.

| Fact | Meaning |
| --- | --- |
| `publication` | A post's publication (its subdomain) |
| `published` | A post's date, from its JSON-LD |
| `handle` | A publication's subdomain, or a profile's handle |
| `feed` | A publication's RSS feed: the feed its page names on its own host, else `https://{name}.substack.com/feed` |
| `blocked` | `true` when Substack refused the request |

## Keys and groups

Substack cards have no keys and join no [groups](../api.md#groups). A publication's `feed` is a
fact only: the feed link gets a card of its own.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.substack]
enabled = false
```

Substack links then get ordinary web cards.

## Examples

- `https://on.substack.com/p/model-behavior-rick-rubin`
- `https://on.substack.com/` (a publication)
- `https://substack.com/@substack` (a profile)
