# Facebook

Facebook pages, posts and videos, from what their public pages carry. No key and no account.
Facebook shows posts and videos to accounts only: such a link gets the card of its shape alone
rather than a failure (below). Event links (`https://www.facebook.com/events/{id}`) are not this source's:
the event module's Facebook adapter reads them (an event page that carries JSON-LD is an event
there, with its dates). A page is social content too: it refreshes within the hour, is removed once it is gone, and its about text is published without e-mail addresses.

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
- A post or video: nothing. Facebook shows them to accounts only, so the card is a link rule:
  the link's root and shape alone, "Post on facebook.com" (with its page as `author`) or
  "Video on facebook.com".

A request Facebook refuses (the sign-in redirect, a challenge, 401 or 403) gives the card of the
link's shape alone with the `blocked` fact: the link still resolves, groups and counts. So does a
page Facebook withholds where the request came from (451): it is not `missing`. A refusal on a
refresh keeps the card read before, as a wall on a web page does.

A card takes its name, text and picture only from the object's own page: when the page read (its `og:url`, else the address the fetch ended at) names another object, as after a redirect, the card is the plain one of the link's shape, as for a page without tags.

## Card

A page is an `Organization` named by its name, with its about text and its picture. A post is
a `SocialMediaPosting`, a video a `VideoObject`, each named by its shape.

| Fact | Meaning |
| --- | --- |
| `author` | A post's page |
| `handle` | A page's name |
| `blocked` | `true` when Facebook refused the request |

## Examples

- `https://www.facebook.com/facebook` (a page)
