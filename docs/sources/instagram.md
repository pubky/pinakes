# Instagram

Instagram posts, reels and profiles, from their pages' Open Graph tags when Instagram writes
them for a request without an account. No key and no account; Instagram's oEmbed endpoint needs
an app token and is not used. Instagram shows almost everything to accounts only: such a link
gets a plain card rather than a failure (below). Posts and reels are social content: the card
quotes at most 200 characters and refreshes within the hour. A profile is social content too: it
refreshes within the hour, is removed once it is gone, and its bio is published without e-mail
addresses.

## Links

A profile is named by its name, a post or reel by its code: `https://www.instagram.com/{name}/`,
`https://www.instagram.com/p/{code}/` and `https://www.instagram.com/reel/{code}/` are the
roots. The host without `www.`, instagr.am, a capitalised name, a missing trailing slash,
`/tv/{code}/` and a post or reel address with its author's name first (`/{name}/p/{code}/`,
`/{name}/reel/{code}/`) are aliases of them. Stories, tags and explore pages are not read.

## What it reads

- A profile: `GET https://www.instagram.com/{name}/`. Instagram redirects a request without an
  account to its sign-in page: the card is the plain one, "Profile on instagram.com", with the
  name and the `blocked` fact. When Instagram writes the page's tags
  (`Name (@name) • Instagram photos and videos`), the card has the name, bio and picture.
- A post or reel: `GET` of its root. A public post's page carries its tags
  (`Name on Instagram: "caption"`, the image, and its own address in `og:url`): the card has the
  caption, author and image, taken only when that address has the link's code. A removed or
  private post, and a reel that does not exist, answer 200 with a page without these tags: the
  card is the plain one, "Post on instagram.com" or "Video on instagram.com". On a card read
  before, such a page counts as a missing observation: the card stays until a confirming read
  finds the same, then it becomes the plain one.

A request Instagram refuses (the sign-in redirect, a challenge, 401 or 403) gives the card of
the link's shape alone with the `blocked` fact: the link still resolves, groups and counts. So
does a page Instagram withholds where the request came from (451): it is not `missing`. A
refusal on a refresh keeps the card read before, as a wall on a web page does.

## Card

A profile is a `Person`, a post a `SocialMediaPosting`, a reel a `VideoObject`.

Instagram serves a reel under both forms (`/p/{code}/` and `/reel/{code}/`, one code), but this
source keeps them apart: `/p/` names a post and `/reel/` a reel, and the link alone cannot say
whether a `/p/` code is a reel. One reel linked both ways is therefore two cards, never one card
for two different things.

| Fact | Meaning |
| --- | --- |
| `author_name` | A post's or reel's author, as its page names them |
| `handle` | A profile's name |
| `blocked` | `true` when Instagram refused the request |

## Examples

- `https://www.instagram.com/p/C2vXx0yPvYq/`
- `https://www.instagram.com/instagram/` (a profile)
