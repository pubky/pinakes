# X

X (Twitter) posts and profiles. No key and no account: a post comes from X's oEmbed endpoint,
a profile from its page's Open Graph tags. Posts are social content: the card quotes at most
200 characters, refreshes within the hour and is removed once the post is gone.

## Links

A post is named by its author's handle and its id, a profile by the handle:
`https://x.com/{handle}/status/{id}` and `https://x.com/{handle}` are the roots. twitter.com,
the `www.` and `mobile.` hosts, a capitalised handle and anything after the id (a photo's
number) are aliases of them. `https://x.com/i/web/status/{id}` and `https://x.com/i/status/{id}`
name a post without its author: the root is known once X names the author.

## What it reads

- A post: `GET https://publish.x.com/oembed?url={root}&omit_script=true&dnt=true`, the
  documented oEmbed endpoint. A 404 is `missing`.
- A profile: `GET https://x.com/{handle}`, its Open Graph tags. A 404 is `missing`.

A request X refuses (a sign-in redirect, a challenge, 401 or 403) gives the card of the link's
shape alone, "Post on x.com" or "Profile on x.com", with the `blocked` fact: the link still
resolves, groups and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does. So does a profile page X withholds where the request came from
(451): it is not `missing`.

## Card

A post is a `SocialMediaPosting` named by the start of its text (else "Post by {author}"),
with its text as description; the oEmbed answer carries no image. A profile is a `Person`
named by its display name, with its bio and its avatar; the page's title must name the
handle the link does (`Name (@handle) on X`), else the card is the plain one.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | For a post, the author's handle and display name |
| `published` | The post's date (`YYYY-MM-DD`, as the oEmbed answer gives it) |
| `handle` | For a profile |
| `blocked` | `true` when X refused the request |

## Examples

- `https://x.com/jack/status/20`
- `https://x.com/X` (a profile)
