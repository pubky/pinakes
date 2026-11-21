# Instagram

Instagram posts, reels and profiles, from their pages' Open Graph tags when Instagram writes
them for a request without an account. No key and no account; Instagram's oEmbed endpoint
needs an app token and is not used. Instagram shows almost everything to accounts only: such a
link gets a plain card rather than a failure (below). Posts and reels are social content: the
card quotes at most 200 characters, refreshes within the hour and is removed once the post is
gone.

## Links

A profile is named by its name, a post or reel by its code: `https://www.instagram.com/{name}/`,
`https://www.instagram.com/p/{code}/` and `https://www.instagram.com/reel/{code}/` are the
roots. The host without `www.`, instagr.am, a capitalised name, a missing trailing slash and
`/tv/{code}/` are aliases of them. Stories, tags and explore pages are not read.

## What it reads

- A profile: `GET https://www.instagram.com/{name}/`. Instagram redirects a request without an
  account to its sign-in page: the card is the plain one, "Profile on instagram.com", with the
  name and the `blocked` fact. When Instagram writes the page's tags
  (`Name (@name) • Instagram photos and videos`), the card has the name, bio and picture.
- A post or reel: `GET` of its root. Instagram names the post only to a browser: the card is
  the plain one, "Post on instagram.com" or "Video on instagram.com". When Instagram writes
  the page's tags (`Name on Instagram: "caption"`), the card has the caption, author and
  image.

A request Instagram refuses (the sign-in redirect, a challenge, 401 or 403) gives the card of
the link's shape alone with the `blocked` fact: the link still resolves, groups and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does. So
does a page Instagram withholds where the request came from (451): it is not `missing`.

## Card

A profile is a `Person`, a post a `SocialMediaPosting`, a reel a `VideoObject`.

Instagram serves a reel under both forms (`/p/{code}/` and `/reel/{code}/`, one code), but this
source keeps them apart: `/p/` names a post and `/reel/` a reel, and the link alone cannot say
whether a `/p/` code is a reel. One reel linked both ways is therefore two cards, never one card
for two different things.

| Fact | Meaning |
| --- | --- |
| `author_name` | A post's author, when the page names one |
| `handle` | A profile's name |
| `blocked` | `true` when Instagram refused the request |

## Examples

- `https://www.instagram.com/p/C2vXx0yPvYq/`
- `https://www.instagram.com/instagram/` (a profile)
