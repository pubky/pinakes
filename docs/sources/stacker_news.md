# Stacker News

Pinakes reads posts and comments on Stacker News from its GraphQL API, by item ID. No key is
needed. Posts and comments are social content.

| | |
| --- | --- |
| Source | `stacker_news`, official source |
| Links | `stacker.news/items/{id}` and its sub-pages; `stacker.news/items/{post}?commentId={comment}` |
| Fetches | `POST https://stacker.news/api/graphql` with an `item(id: "{id}")` query |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Within the hour (social content) |
| Card types | `DiscussionForumPosting`, `Comment` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.stacker_news] enabled`, on by default |

## Links

Every address of one item is one resource. The root is `https://stacker.news/items/{id}`, and
these links are [aliases](../api.md#aliases) of it:

- the item's sub-pages, such as `https://stacker.news/items/{id}/related`;
- referral forms, `https://stacker.news/items/{id}/r/{name}` and `…/items/{id}?r={name}`;
- the same links on `www.stacker.news`.

An item ID is a positive decimal number of at most 12 digits.

A link to a comment in its thread, `https://stacker.news/items/{post}?commentId={comment}`, is
not an alias from the link alone. Pinakes first reads the comment. If the comment's own record puts
it in the thread of `{post}`, the link becomes an alias of the comment's root,
`https://stacker.news/items/{comment}`. Otherwise the link stands for the post and becomes an alias
of the post's root.

Not read here: territory pages (`stacker.news/~bitcoin`), the front page, and `items` links whose
ID is not a number.

## What it fetches

Each read makes one request, `POST https://stacker.news/api/graphql`, with this query:

```graphql
{ item(id: "{id}") { id title url text createdAt user { name } sub { name }
  root { id title } parentId noteId ncomments sats deletedAt } }
```

A link to a comment in its thread makes up to two of these requests: one for the comment and,
when the comment is not in that thread, one for the post.

The API answers "no such item" with `item: null`. That item, and one with a deletion time, is
`missing`. These answers are errors, and the read is retried:

- any status other than 200;
- an answer with GraphQL errors (the error's own text is not kept);
- an answer without `data`, or `data` without `item`;
- an answer for another item.

## Card

An item with a title is a post: a `DiscussionForumPosting` named by its title. An item without a
title is a comment: a `Comment` named after its thread, for example
"Comment on “Sword < Pen < Home Mining < High Hashrate Home Mining”".

- `description` is an excerpt of the item's Markdown text, as plain text of at most 200
  characters, without images.
- `image` is Stacker News's own capture of the item, `https://capture.stacker.news/items/{id}`.

| Fact | Meaning |
| --- | --- |
| `author` | The author's user name |
| `territory` | The post's territory, for example `bitcoin` |
| `published` | When the item was posted, as the API gives it |
| `comment_count` | The number of comments |
| `sats` | The bitcoin the item received, an amount in bitcoin base units (₿) |
| `link` | The link a post points at (another object, not this one) |
| `thread` | For a comment, the page of its thread's root post |
| `nostr` | The item's Nostr cross-post, `nostr:…` |
| `page_url` | The item's page on Stacker News |

## Keys and groups

Stacker News items have no keys and join no [groups](../api.md#groups). The post's `link` and the
`nostr` cross-post are facts only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.stacker_news]
enabled = false
```

Stacker News links then get ordinary web cards.

## Examples

- `https://stacker.news/items/1596515`
- `https://stacker.news/items/1596515?commentId=1596517` (a comment)
