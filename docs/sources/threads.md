# Threads

Threads posts and profiles, from their pages' Open Graph tags. No key and no account. Posts are
social content: the card quotes at most 200 characters, refreshes within the hour and is removed
once the post is gone.

## Links

A profile is named by its handle, a post by its author's handle and its code:
`https://www.threads.com/@{handle}` and `https://www.threads.com/@{handle}/post/{code}` are the
roots. threads.net, the hosts without `www.` and a capitalised handle are aliases of them.
`https://www.threads.com/t/{code}` names a post without its author: the root is known once the
page gives its own address (its canonical link, or the address a redirect ends at, for the same
code). The author's
handle is taken from that address, else from the last `(@handle)` of the page's title.

## What it reads

- A post: `GET https://www.threads.com/@{handle}/post/{code}`, its Open Graph tags. A post
  that does not exist sends the fetch to the home page (`/?error=invalid_post`): `missing`.
- A profile: `GET https://www.threads.com/@{handle}`, its Open Graph tags. A profile that does
  not exist, like anything Threads shows to accounts only, redirects to its sign-in page.

A request Threads refuses (a sign-in redirect, a challenge, 401 or 403) gives the card of the
link's shape alone, "Post on threads.com" or "Profile on threads.com", with the `blocked`
fact: the link still resolves, groups and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does. So does a page Threads withholds where the
request came from (451): it is not `missing`.

## Card

A post is a `SocialMediaPosting` named by the start of its text (else "Post by {author}"),
with its text as description and its first image. A profile is a `Person` named by its
display name, with its bio (the page's description after its follower and post counts) and
its avatar; the page's title must name the handle the link does (`Name (@handle) • Threads`),
else the card is the plain one.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | For a post, the author's handle and display name |
| `handle` | For a profile |
| `blocked` | `true` when Threads refused the request |

## Examples

- `https://www.threads.com/@threads/post/Der1HO7miy_`
- `https://www.threads.com/@threads` (a profile)
