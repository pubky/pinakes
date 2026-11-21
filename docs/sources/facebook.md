# Facebook

Facebook pages, posts and videos, from what their public pages carry. No key and no account.
Facebook shows posts and videos to accounts only: such a link gets the card of its shape alone
rather than a failure (below). Event links (`https://www.facebook.com/events/{id}`) are not this source's:
the event module's Facebook adapter reads them (an event page that carries JSON-LD is an event
there, with its dates).

## Links

A page is named by its name, a post by its page and id, a video by its id:
`https://www.facebook.com/{name}`, `https://www.facebook.com/{name}/posts/{id}` and
`https://www.facebook.com/watch/?v={id}` are the roots. m., web. and mbasic.facebook.com,
fb.com, a capitalised name and a reel's address (`/reel/{id}`) are aliases of them. Groups,
photos, share links and `profile.php` links are not read.

## What it reads

- A page: `GET https://www.facebook.com/{name}`, its Open Graph tags (the name, and a description of
  its counts and about text in the page's language). A page that does not exist, like anything shown
  to accounts only, redirects to the sign-in page. An answer without tags gives the plain card,
  "Profile on facebook.com", with the name: it does not say the page is gone, and a stored card that
  says more (another name, text, a picture or facts) stays, as last read, until a second read an
  hour later (`limits.missing_ttl_secs`) answers the same, as a missing answer is confirmed: then the
  plain card replaces it.
- A post or video: `GET` of its root. Nothing is read from the answer: the card is the link's
  shape alone, "Post on facebook.com" (with its page) or "Video on facebook.com". What comes
  back without an account is the sign-in redirect, a refused request: that card has the
  `blocked` fact (below). A 404 or 410 is `missing`.

A request Facebook refuses (the sign-in redirect, a challenge, 401 or 403) gives the card of
the link's shape alone with the `blocked` fact: the link still resolves, groups and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does. So
does a page Facebook withholds where the request came from (451): it is not `missing`.

## Card

A page is an `Organization` named by its name, with its about text and its picture. A post is
a `SocialMediaPosting`, a video a `VideoObject`.

| Fact | Meaning |
| --- | --- |
| `author` | A post's page |
| `handle` | A page's name |
| `blocked` | `true` when Facebook refused the request |

## Examples

- `https://www.facebook.com/facebook` (a page)
