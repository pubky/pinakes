# Telegram

Pinakes reads public Telegram channels and their posts from t.me's public pages. No key or account
is needed. Channels and posts are social content.

| | |
| --- | --- |
| Source | `telegram`, official source |
| Links | `t.me/{name}`, `t.me/{name}/{n}`, `t.me/s/{name}`, `t.me/s/{name}/{n}`; the same on `telegram.me` |
| Fetches | `GET https://t.me/{name}/{n}?embed=1` for a post, `GET https://t.me/{name}` for a channel |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `t.me` |
| Refresh | Within the hour (social content) |
| Card types | `SocialMediaPosting` (post), `Person` (channel) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.telegram] enabled`, on by default |

## Links

The roots are:

- a channel, or any other public name: `https://t.me/{name}`;
- a post: `https://t.me/{name}/{n}`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `www.t.me`, `telegram.me` and `www.telegram.me`, or with `http`;
- the preview addresses `https://t.me/s/{name}` and `https://t.me/s/{name}/{n}`;
- a name with capitals (roots are lower case), an address with a trailing slash or query
  parameters (such as `?single`).

A name is 5 to 32 letters, digits and underscores. `{n}` is a number of at most 20 digits.

Not read here: invite links (`/+…`, `/joinchat/…`), private channels (`/c/…`), a post's sub-pages
and t.me's own pages (such as `/share/…`, `/addstickers/…` and `/proxy`).

## What it fetches

### Posts

One request, `GET https://t.me/{name}/{n}?embed=1`: the page Telegram's post widget loads. A widget
that reports an error ("Post not found") is `missing`, and so is a 404 or 410. A page without a
post widget is an error, and the read is retried.

### Channels

One request, `GET https://t.me/{name}`, for the page's Open Graph tags. A 404 or 410 is `missing`.

For either, any status that is neither listed here nor a refusal is an error, and the read is
retried. A 408, a 429 or a server error waits at least a minute.

### Refused requests

t.me refuses a request with a sign-in redirect, a challenge page, or a 401, 403 or 451. A link never
read before then gets the card of its shape alone, "Post on t.me" or "Profile on t.me", with the
description "Blocked by the site." and the `blocked` fact. The link still resolves, groups and
counts. A refusal on a refresh keeps the card read before, and the link is asked again in an hour.

## Card

A card takes its name, text and image only from the object's own page: the address the fetch ended
at must name the same channel and post. Any other page gives the plain card, "Post on t.me" with
the `author` fact or "Profile on t.me" with the `handle` fact. A stored card that says more stays
until a second read, an hour later by default (`limits.missing_ttl_secs`, at least an hour),
answers the same; then the plain card replaces it.

### Posts

A post is a `SocialMediaPosting`.

- `name` is the first 120 characters of the text on one line, or "Post by {channel}" for a post
  without text.
- `description` is the first 200 characters of the text, with its line breaks.
- `image` is the post's photo or video still.

### Channels

A channel is a `Person` named by its title. `description` is the first 200 characters of its
description, without e-mail addresses. `image` is its photo.

A name without a public page (a user, a private channel or no such name) answers "Telegram: Contact
@name". It gives the plain card, "Profile on t.me" with the `handle` fact, which replaces a stored
card at once.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the channel's name, in lower case |
| `author_name` | For a post: the channel's title |
| `published` | For a post: its time, as the widget gives it (RFC 3339) |
| `views` | For a post: its views, as the widget shows them (`2.82M`) |
| `handle` | For a channel: its name, in lower case |
| `blocked` | `true` when t.me refused the request |

## Keys and groups

Telegram cards have no keys. Names and post numbers join nothing.

## Credit

The card credits nothing. A post's channel is in `author` and `author_name`.

## Turn it off

```toml
[sources.telegram]
enabled = false
```

Telegram links then get ordinary web cards.

## Examples

- `https://t.me/telegram/300`
- `https://t.me/telegram` (a channel)
