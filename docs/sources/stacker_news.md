# Stacker News

Posts and comments on Stacker News, read from its GraphQL API by item id. No key; no published
limit (one request per second). Posts and comments are social content: the card quotes at most
200 characters, refreshes within the hour and is removed once the item is gone.

## Links

`https://stacker.news/items/{id}` is the root; the item's sub-pages and referral forms
(`/items/{id}/r/{name}`, `?r={name}`) are aliases of it. A link to a comment in its thread,
`/items/{post}?commentId={comment}`, becomes an alias of the comment's own item after its first
fetch, when the comment's record puts it in that thread; otherwise it stands for the thread.

## What it reads

`POST https://stacker.news/api/graphql` with `{ item(id: "{id}") { … } }`: title, text, author,
territory, time, counts, the discussed link, the thread's root and its Nostr cross-post. An item
that does not exist or was deleted is `missing`.

## Card

A post is a `DiscussionForumPosting` named by its title; a comment is a `Comment` named after its
thread ("Comment on “…”"). The description is an excerpt of its text; the image is Stacker
News's own capture of the item (`https://capture.stacker.news/items/{id}`).

| Fact | Meaning |
| --- | --- |
| `author` | The author's name |
| `territory` | The post's territory, e.g. `bitcoin` |
| `published` | RFC 3339 |
| `comment_count`, `sats` | Its comments and the sats it received |
| `link` | The link a post discusses (another object, not this one) |
| `thread` | For a comment, its thread's root post |
| `nostr` | Its Nostr cross-post, `nostr:…` |
| `page_url` | Its page on Stacker News |

## Examples

- `https://stacker.news/items/1596515`
- `https://stacker.news/items/1596515?commentId=1596517` (a comment)
