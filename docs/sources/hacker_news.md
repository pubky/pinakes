# Hacker News

Hacker News stories, comments, jobs and polls, read from the official Firebase API by item id.
No key; the API publishes no limit (one request per second). Items are social content: the card
quotes at most 200 characters, refreshes within the hour and is removed once the item is gone.

## Links

`https://news.ycombinator.com/item?id={id}` is the root; the same item's other pages
(`&p=2`), its hnsignals.com page `https://hnsignals.com/signal/{id}` (which names items by the
same id) and the API's `https://hacker-news.firebaseio.com/v0/item/{id}.json` are aliases of it.

## What it reads

`GET https://hacker-news.firebaseio.com/v0/item/{id}.json` (well under 1 KB). An item that does
not exist or was deleted is `missing`.

## Card

A story, job or poll is a `DiscussionForumPosting` named by its title; a comment is a `Comment`
named after its author ("Comment by …"). The description is an excerpt of its text.

| Fact | Meaning |
| --- | --- |
| `author` | The author's user name |
| `item_type` | `story`, `comment`, `job`, `poll` or `pollopt` |
| `published` | RFC 3339 |
| `score`, `comment_count` | For a story |
| `link` | The link a story discusses (another object, not this one) |
| `parent` | For a comment, the item it answers |
| `page_url` | Its page on Hacker News |

## Examples

- `https://news.ycombinator.com/item?id=8863`
- `https://hnsignals.com/signal/2921983` (a comment, by its hnsignals page)
