# LinkedIn

Pinakes reads LinkedIn profiles and company pages from their pages' Open Graph tags. A post's card
comes from the link alone and nothing is fetched, because LinkedIn answers a post's page with its
sign-in invitation. No key or account is needed. LinkedIn refuses many requests, and such a link
gets a plain card. Profiles, company pages and posts are social content.

| | |
| --- | --- |
| Source | `linkedin`, official source |
| Links | `linkedin.com/in/{slug}`, `linkedin.com/company/{slug}`, `linkedin.com/school/{slug}`, `linkedin.com/posts/{slug}-activity-{id}-{tag}`, `linkedin.com/feed/update/urn:li:activity:{id}` |
| Fetches | `GET https://www.linkedin.com/in/{slug}` or `GET https://www.linkedin.com/company/{slug}`; nothing for a post |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.linkedin.com` |
| Refresh | Within the hour (social content); a post's refresh makes no request |
| Card types | `Person` (profile), `Organization` (company or school), `SocialMediaPosting` (post) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt of the about text; a profile or company page is purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.linkedin] enabled`, on by default |

## Links

The roots are:

- a profile: `https://www.linkedin.com/in/{slug}`;
- a company or school: `https://www.linkedin.com/company/{slug}`;
- a post: the address the link spells, `https://www.linkedin.com/posts/{slug}-activity-{id}-{tag}`
  or `https://www.linkedin.com/feed/update/urn:li:activity:{id}`.

The two post forms are not aliases of each other, because the activity ID alone cannot spell the
`/posts/` address. A `/posts/` address without words, `{author}_activity-{id}-{tag}`, is read too.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `linkedin.com`, with any host prefix of up to five lower-case letters (a
  country's site, such as `de.linkedin.com`), or with `http`;
- `/school/{slug}`, an alias of the company;
- a profile or company slug with capitals (those roots are lower case; a `/posts/` slug keeps its
  case), an address with a trailing slash or query parameters.

A slug is 2 to 120 letters, digits, `-`, `_` and percent-encoded characters. An activity ID is a
number of at most 20 digits.

Not read here: jobs, `urn:li:share:…` links and LinkedIn's other pages.

## What it fetches

A profile's or company's read makes one request, for the root's page. A 404 or 410 is `missing`. Any
status that is neither listed here nor a refusal is an error, and the read is retried. A 429 waits
at least the time its `Retry-After` header names (up to an hour), or 5 minutes when it names none.
A 408 or a server error waits at least a minute.

A post makes no request.

### Refused requests

LinkedIn refuses a request with its own status 999, a sign-in redirect, a challenge page, or a 401,
403 or 451. A link never read before then gets the card of its shape alone, "Profile on
linkedin.com" (an `Organization` for a company), with the description "Blocked by the site." and the
`blocked` fact. The link still resolves, groups and counts. A refusal on a refresh keeps the card
read before. After a refusal the link is asked again a day later, then 2, 4 and 7 days later, and
every 7 days while the refusals go on ([refusals](../operations.md#failures-and-retries)).

## Card

### Profiles and companies

The tags are read as written, because LinkedIn's JSON-LD describes recent posts and not the page.

| | Profile | Company or school |
| --- | --- | --- |
| Type | `Person` | `Organization` |
| Title read | `Name - Headline \| LinkedIn` | `Name \| LinkedIn` |
| `name` | The display name | The name |
| `description` | The about text, else the headline | The about text after `N followers on LinkedIn.` |
| `image` | The photo | The logo |

The about text is cut to 200 characters. A headline shown as the description is not cut. E-mail
addresses are removed from the description.

The card takes these only from the object's own page: the address the page gives itself (`og:url`,
else the address the fetch ended at) must name the same profile or company, under any of its
spellings.

Any other page, or one without tags (a consent page or LinkedIn's generic page), does not say that
the object is gone. It gives the plain card, "Profile on linkedin.com" with the `handle` fact. A
stored card that says more stays until a second read, an hour later by default
(`limits.missing_ttl_secs`, at least an hour), answers the same; then the plain card replaces it.

### Posts

A post is a `SocialMediaPosting` named "Post on linkedin.com". A `/posts/` link also gives it its
author. It has no text or image.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the author, as its `/posts/` address spells it (the part before the first `_`) |
| `handle` | For a profile or company: its slug, in lower case |
| `headline` | For a profile: its headline |
| `followers` | For a company: its follower count |
| `blocked` | `true` when LinkedIn refused the request |

## Keys and groups

LinkedIn cards have no keys. Slugs and activity IDs join nothing.

## Credit

The card credits nothing. A post's author is in `author`.

## Turn it off

```toml
[sources.linkedin]
enabled = false
```

LinkedIn links then get ordinary web cards.

## Examples

- `https://www.linkedin.com/company/linkedin`
- `https://www.linkedin.com/in/williamhgates` (a profile)
