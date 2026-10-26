# LinkedIn

LinkedIn profiles, company pages and posts, from their pages' Open Graph tags. No key and no
account. LinkedIn shows most of its pages to accounts only: a refused request gives a plain
card rather than a failure (below).

## Links

A profile is named by its slug, a company (or school) by its slug, a post by its activity id:
`https://www.linkedin.com/in/{slug}`, `https://www.linkedin.com/company/{slug}` and
`https://www.linkedin.com/feed/update/urn:li:activity:{id}` are the roots. The host without
`www.` or with a country prefix (`de.linkedin.com`), a capitalised slug, `/school/{slug}` and a
post's `/posts/{slug}-activity-{id}-{tag}` address are aliases of them.

## What it reads

- A profile: `GET https://www.linkedin.com/in/{slug}`, its Open Graph tags
  (`Name - Headline | LinkedIn`; the description joins headline, about, experience, education
  and location with ` · `).
- A company: `GET https://www.linkedin.com/company/{slug}`, its Open Graph tags
  (`Name | LinkedIn`; the description starts `Name | N followers on LinkedIn.`). A 404 is
  `missing`.
- A post: `GET` of its `/posts/` address when the link spelled one (the address LinkedIn shows
  without an account), else its root. A page offering to sign in instead of the post gives
  the plain card.

The tags are read as written: LinkedIn's JSON-LD describes recent posts, not the page.
LinkedIn answers many requests without an account with HTTP 999 or its sign-in page; such a
request gives the card of the link's shape alone, "Profile on linkedin.com" or "Post on
linkedin.com", with the `blocked` fact: the link still resolves, groups and counts.

## Card

A profile is a `Person` named by its display name, with its about text (else its headline)
and its photo. A company is an `Organization` named by its name, with its about text and its
logo. A post is a `SocialMediaPosting` named by its title, with its text and image.

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
