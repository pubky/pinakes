# Mastodon

Pinakes reads posts on any Mastodon server from that server's own API, by the post's ID. No key is
needed. Posts are social content.

| | |
| --- | --- |
| Source | `mastodon`, official source |
| Links | `https://{server}/@{account}/{id}`, `https://{server}/users/{account}/statuses/{id}`, on any server |
| Fetches | `GET https://{server}/api/v1/statuses/{id}` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on each server |
| Refresh | Within the hour (social content) |
| Card types | `SocialMediaPosting` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.mastodon] enabled`, on by default |

## Links

The source reads `https` links of two forms on any host, without a port:

- `https://{server}/@{account}/{id}`, where `{account}` is `name` or `name@server`;
- `https://{server}/users/{account}/statuses/{id}`, where `{account}` is one of the server's own
  accounts and holds no `@`.

`{id}` is a number of at most 20 digits.

A post's root is its ActivityPub ID, which the API names (for example
`https://mastodon.social/users/Gargron/statuses/1`). After its first successful read, an address of
the post on its own server becomes an [alias](../api.md#aliases) of that ID. The ID is then read in
the link's place. If the ID has neither of the two forms above, the web route reads it.

Only the ID's own server can say that two links are one post. An answer that names an ID on another
host therefore aliases nothing: the link keeps its own card, with the ID as the `activitypub_id`
fact. A server's copy of another server's post (`https://{other}/@{account}@{server}/{id}`) is such
a link.

The post must belong to the account the link names: the answer's `account.acct`, compared without
case. `@{account}@{server}` may also name an account of `{server}` itself. A link that names
another account gets an ordinary web card.

Not read here: profiles (`https://{server}/@{account}`), sub-pages such as
`/@{account}/{id}/reblogs`, and `http` links.

## What it fetches

One request per read: `GET https://{server}/api/v1/statuses/{id}`.

| Answer | Result |
| --- | --- |
| 200 with a post of the link's account | The post's card |
| 404 or 410, JSON with an `error` text, from a server that answered with a Mastodon post within the last day | `missing` |
| The same from any other server, whose program is unknown | An ordinary web card |
| A JSON refusal: 401, 403, 405, or a 404 without an `error` text | An ordinary web card for this link only |
| Any answer that is not JSON | An ordinary web card, and the server is remembered as not Mastodon |
| JSON that is no post, or a post of another account | An ordinary web card |
| A post with another ID, or any other status | An error: the read is retried and a stored card stays |
| A challenge page or a sign-in redirect | A failed read ([access walls](../design.md#access-walls)) |
| 408, 429 or a server error | Retried after at least a minute |

A server's answer, Mastodon or not, is remembered for a day, in memory and for at most 10,000
servers. While a server is remembered as not Mastodon, its links get ordinary web cards without
asking the API. A web card reads ActivityPub where the page offers it.

## Card

The card is a `SocialMediaPosting`.

- `site` is the server's host.
- `name` is the first 120 characters of the text on one line, or "Post by {author}" for a post
  without text.
- `description` is the first 200 characters of the text, with its line breaks and paragraphs.
- A content warning takes the place of the text it hides, in both `name` and `description`.
- `image` is the first attachment's preview, unless the post is marked sensitive; else the author's
  avatar.
- `embed` is the post's own frame on its server, `https://{server}/@{account}/{id}/embed`
  ([embed](../api.md#embed)). A copy seen through another server has none.

### Authors who have not opted in to search

An author has not opted in to search when the account's `indexable` is `false` (no full-text search
of its public posts, Mastodon's default) or its `noindex` is `true` (search engines asked not to
index the profile). The post keeps its normal card, with the `opted_out` fact, and:

- a text search (`q=`) never finds it, and related results never list it;
- its change history keeps no text. What was kept before the card was marked is deleted, and its
  change lines carry only their kind, fields and time;
- spellings saved before they became its aliases keep only the line that names the post.

### Deleted posts

A post confirmed gone keeps nothing of itself: no name, text, image or fact (the content warning
included) and no change history ([social content](../design.md#social-content)).

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | The author's account (`name`, or `name@server` for another server's account) |
| `author_name` | The author's display name |
| `published` | When the post was created, as the API gives it (RFC 3339) |
| `replies`, `reblogs`, `favourites` | Its counts |
| `content_warning` | The post's content warning, if it has one |
| `activitypub_id` | Its ActivityPub ID, the root of its addresses on its own server |
| `page_url` | Its page, when that is on the server the link names |
| `opted_out` | `true` when the author has not opted in to search |

## Keys and groups

Mastodon cards have no keys. The `activitypub_id` of a copy is a fact only: the copy and the
original are two resources and join nothing.

## Credit

The card credits nothing. The author is in `author` and `author_name`.

## Turn it off

```toml
[sources.mastodon]
enabled = false
```

Mastodon links then get ordinary web cards.

## Examples

- `https://mastodon.social/@Gargron/1`
