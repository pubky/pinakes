# Threads

Pinakes reads Threads posts and profiles from their pages' Open Graph tags. No key or account is
needed. Posts and profiles are social content.

| | |
| --- | --- |
| Source | `threads`, official source |
| Links | `threads.com/@{handle}`, `threads.com/@{handle}/post/{code}`, `threads.com/t/{code}`; the same on `threads.net` |
| Fetches | `GET https://www.threads.com/@{handle}`, `GET https://www.threads.com/@{handle}/post/{code}` or `GET https://www.threads.com/t/{code}` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.threads.com` |
| Refresh | Within the hour (social content). A link the site refuses: after 1 day, doubling to every 7 days ([refusals](../operations.md#failures-and-retries)) |
| Card types | `SocialMediaPosting` (post), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.threads] enabled`, on by default |

## Links

The roots are:

- a post: `https://www.threads.com/@{handle}/post/{code}`;
- a profile: `https://www.threads.com/@{handle}`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `threads.com`, `threads.net` and `www.threads.net`, or with `http`;
- a handle with capitals (roots are lower case).

`https://www.threads.com/t/{code}` names a post without its author. It becomes an alias of the root
after a read in which the page gives its own address: its canonical link, or the address a redirect
ended at, for the same code. A post link under another handle gets its root the same way.

A handle is at most 30 letters, digits, `_`, `.` and `-`. A code is 6 to 32 letters, digits, `_`
and `-`.

Not read here: a profile's sub-pages (such as `/@{handle}/followers`), search pages and addresses
without the `@`.

## What it fetches

One request per read, for the page.

- A post that does not exist sends the fetch to the home page (`/?error=invalid_post`): `missing`.
- A 404 or 410 is `missing`.
- Any status that is neither listed here nor a refusal is an error, and the read is retried.
- A 429 waits at least the time Threads names (up to an hour), or 5 minutes when it names none.
- A 408 or a server error waits at least a minute.

### Refused requests

Threads refuses a request with a sign-in redirect, a challenge page, or a 401, 403 or 451. A
profile that does not exist redirects to the sign-in page too, so it reads as refused and not as
`missing`.

A link never read before then gets the card of its shape alone, "Post on threads.com" or "Profile on
threads.com", with the description "Blocked by the site." and the `blocked` fact. The link still
resolves, groups and counts. A refusal on a refresh keeps the card read before. After a refusal the
link is asked again after a day, then after 2, 4 and 7 more days, and every 7 days while one refusal
follows another ([refusals](../operations.md#failures-and-retries)).

## Card

### Posts

A post is a `SocialMediaPosting`.

- `name` is the first 120 characters of the text on one line, or "Post by {author}" for a post
  without text.
- `description` is the first 200 characters of the text, with its line breaks.
- `image` is the page's image.
- The author's handle comes from the address the page gives itself, else from the last `(@handle)`
  of the page's title.

### Profiles

A profile is a `Person` named by its display name. `description` is the bio: the first 200
characters of the page's description after its follower and thread counts, without e-mail
addresses. `image` is the avatar.

The page's title must name the link's handle (`Name (@handle) • Threads`). Otherwise the card is
the plain one, "Profile on threads.com" with the `handle` fact, and it replaces a stored card at
once.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the author's handle, in lower case |
| `author_name` | For a post: the author's display name |
| `handle` | For a profile: its handle, in lower case |
| `blocked` | `true` when Threads refused the request |

## Keys and groups

Threads cards have no keys. Handles and post codes join nothing.

## Credit

The card credits nothing. A post's author is in `author` and `author_name`.

## Turn it off

```toml
[sources.threads]
enabled = false
```

Threads links then get ordinary web cards.

## Examples

- `https://www.threads.com/@threads/post/Der1HO7miy_`
- `https://www.threads.com/@threads` (a profile)
