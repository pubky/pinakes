# X

Pinakes reads X (Twitter) posts from X's oEmbed endpoint and profiles from their pages' Open Graph
tags. No key or account is needed. Posts and profiles are social content.

| | |
| --- | --- |
| Source | `x`, official source |
| Links | `x.com/{handle}/status/{id}`, `x.com/{handle}`, `x.com/i/web/status/{id}`, `x.com/i/status/{id}`; the same on `twitter.com` |
| Fetches | `GET https://publish.x.com/oembed?omit_script=true&dnt=true&url={post}` for a post, `GET https://x.com/{handle}` for a profile |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `publish.x.com` and `x.com` |
| Refresh | Within the hour (social content) |
| Card types | `SocialMediaPosting` (post), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt of a post, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.x] enabled`, on by default |

## Links

The roots are:

- a post: `https://x.com/{handle}/status/{id}`;
- a profile: `https://x.com/{handle}`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `www.x.com`, `mobile.x.com`, `twitter.com`, `www.twitter.com` and
  `mobile.twitter.com`, or with `http`;
- a handle with capitals (roots are lower case);
- a post's address with anything after the ID, such as `/photo/1`.

`https://x.com/i/web/status/{id}` and `https://x.com/i/status/{id}` name a post without its author.
They become aliases of the root after a read in which X names the author.

A handle is 1 to 15 letters, digits and underscores. A post ID is a number of at most 20 digits.

Not read here: X's own pages (such as `/home`, `/search`, `/explore` and `/i/lists/…`) and a
profile's sub-pages (such as `/{handle}/likes`).

## What it fetches

### Posts

One request to X's oEmbed endpoint. `url` is the post's root, or
`https://x.com/i/web/status/{id}` for a link without an author. A 400 or 404 is `missing`. An
answer for another post, an answer that is not a JSON object and any other status are errors, and
the read is retried.

### Profiles

One request for the profile's page, `https://x.com/{handle}`. A 404 or 410 is `missing`. Any status
that is neither listed here nor a refusal is an error, and the read is retried.

A 408, a 429 or a server error, to either request, waits at least a minute.

### Refused requests

X refuses a request with a sign-in redirect, a challenge page, or a 401 or 403; for a profile's
page, a 451 counts too. A link never read before then gets the card of its shape alone, "Post on
x.com" or "Profile on x.com", with the description "Blocked by the site." and the `blocked` fact.
The link still resolves, groups and counts. A refusal on a refresh keeps the card read before, and
the link is asked again in an hour.

## Card

### Posts

A post is a `SocialMediaPosting`.

- `name` is the first 120 characters of the text on one line, or "Post by {author}" for a post
  without text.
- `description` is the first 200 characters of the text, with its line breaks and paragraphs.
- The `pic.twitter.com/…` and `pic.x.com/…` links that X appends at the end of the text for photos
  and videos are left out of both.
- `image` is empty: the oEmbed answer carries none.
- `embed` is X's own frame for the post, from the link alone ([embed](../api.md#embed)).

### Profiles

A profile is a `Person` named by its display name, with its bio, without e-mail addresses, as
`description` and its avatar as `image`.

The card takes these only from the profile's own page, and only when the page's title names the
link's handle (`Name (@handle) on X`). Otherwise it is the plain card: "Profile on x.com" with the
`handle` fact.

- When the fetch ended at another address, a stored card that says more stays until a second read,
  an hour later by default (`limits.missing_ttl_secs`, at least an hour), answers the same.
- When the profile's own page does not name the handle, the plain card replaces the stored one at
  once.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the author's handle |
| `author_name` | For a post: the author's display name |
| `published` | For a post: its date (`YYYY-MM-DD`), as the oEmbed answer shows it |
| `handle` | For a profile: its handle, in lower case |
| `blocked` | `true` when X refused the request |

## Keys and groups

X cards have no keys. Handles and post IDs join nothing.

## Credit

The card credits nothing. A post's author is in `author` and `author_name`.

## Turn it off

```toml
[sources.x]
enabled = false
```

X links then get ordinary web cards. A post's `embed` stays, because it comes from the link alone.

## Examples

- `https://x.com/jack/status/20`
- `https://x.com/X` (a profile)
