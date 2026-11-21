# Instagram

Instagram posts, reels and profiles. No key and no account; Instagram's oEmbed endpoint needs an
app token and is not used. Instagram shows almost everything to accounts only: such a link gets
a plain card rather than a failure (below). Posts and reels are link rules: the card is the
link's root and shape alone, and nothing is asked. A profile is read from its page's Open Graph
tags when Instagram writes them for a request without an account. It is social content: it
refreshes within the hour, is removed once it is gone, and its bio is published without e-mail
addresses.

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
- A post or a reel: nothing. Instagram names a post or reel only to a browser, so the card is
  "Post on instagram.com" or "Video on instagram.com", from the link alone.

A profile request Instagram refuses (the sign-in redirect, a challenge, 401 or 403) gives the
card of the link's shape alone with the `blocked` fact: the link still resolves, groups and
counts. So does a page Instagram withholds where the request came from (451): it is not
`missing`. A refusal on a refresh keeps the card read before, as a wall on a web page does.

## Card

A profile is a `Person`, a post a `SocialMediaPosting`, a reel a `VideoObject`.

Instagram serves a reel under both forms (`/p/{code}/` and `/reel/{code}/`, one code), but this
source keeps them apart: `/p/` names a post and `/reel/` a reel, and the link alone cannot say
whether a `/p/` code is a reel. One reel linked both ways is therefore two cards, never one card
for two different things.

| Fact | Meaning |
| --- | --- |
| `handle` | A profile's name |
| `blocked` | `true` when Instagram refused the request |

## Examples

- `https://www.instagram.com/p/C2vXx0yPvYq/`
- `https://www.instagram.com/instagram/` (a profile)
