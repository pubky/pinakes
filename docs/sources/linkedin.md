# LinkedIn

LinkedIn profiles, company pages and posts, from their pages' Open Graph tags. No key and no
account. LinkedIn shows most of its pages to accounts only: a refused request gives a plain
card rather than a failure (below). A profile or company page is social content too: it refreshes within the hour, is removed once it is gone, and its about text is published without e-mail addresses.

## Links

A profile is named by its slug, a company (or school) by its slug:
`https://www.linkedin.com/in/{slug}` and `https://www.linkedin.com/company/{slug}` are the roots. A post's root is the address the
link spells: `https://www.linkedin.com/posts/{slug}-activity-{id}-{tag}`, the one LinkedIn shows
without an account, or `https://www.linkedin.com/feed/update/urn:li:activity:{id}`. The two are
not aliases of each other: the activity id alone cannot spell the `/posts/` address, and the
URN's page is shown to accounts only. The host without `www.` or with any prefix of up to five
lower-case letters (a country's site, `de.linkedin.com`), a capitalised slug and
`/school/{slug}` are aliases.

## What it reads

- A profile: `GET https://www.linkedin.com/in/{slug}`, its Open Graph tags
  (`Name - Headline | LinkedIn`; the description joins headline, about, experience, education
  and location with ` · `).
- A company: `GET https://www.linkedin.com/company/{slug}`, its Open Graph tags
  (`Name | LinkedIn`; the description starts `Name | N followers on LinkedIn.`). A 404 is
  `missing`.
- A post: nothing. LinkedIn answers a post's page to a request without an account with its
  sign-in invitation, so the card is a link rule: "Post on linkedin.com", with the author the
  `/posts/` address spells.

The tags are read as written: LinkedIn's JSON-LD describes recent posts, not the page. A profile's
or company's page answered without them gives the plain card, "Profile on linkedin.com", with the
slug: it does not say the object is gone, and a stored card that says more (another name, text, a
picture or facts) stays, as last read, until a second read an hour later (`limits.missing_ttl_secs`)
answers the same, as a missing answer is confirmed: then the plain card replaces it. LinkedIn
answers many requests without an account with HTTP 999 or its sign-in page; such a request gives the
card of the link's shape alone, "Profile on linkedin.com", with the
`blocked` fact: the link still resolves, groups and counts. So does a page LinkedIn withholds where
the request came from (451): it is not `missing`. A refusal on a refresh keeps the card read before,
as a wall on a web page does.

## Card

A profile is a `Person` named by its display name, with its about text (else its headline)
and its photo. A company is an `Organization` named by its name, with its about text and its
logo. A post is a `SocialMediaPosting` named by its shape.

| Fact | Meaning |
| --- | --- |
| `author` | A post's author, from its `/posts/` address |
| `handle` | A profile's or company's slug |
| `headline` | A profile's headline |
| `followers` | A company's follower count |
| `blocked` | `true` when LinkedIn refused the request |

## Examples

- `https://www.linkedin.com/company/linkedin`
- `https://www.linkedin.com/in/williamhgates` (a profile)
