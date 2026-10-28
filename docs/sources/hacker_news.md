# Hacker News

Hacker News stories, comments, jobs and polls, read from the official Firebase API by item id.
No key; the API publishes no limit (one request per second). Items are social content: the card
quotes at most 200 characters, refreshes within the hour and is removed once the item is gone.

## Links

`https://news.ycombinator.com/item?id={id}` is the root; the same item's other pages
(`&p=2`), its hnsignals.com page `https://hnsignals.com/signal/{id}` (which names items by the
same id) and the API's `https://hacker-news.firebaseio.com/v0/item/{id}.json` are aliases of it.

## What it reads

`GET https://hacker-news.firebaseio.com/v0/item/{id}.json` (well under 1 KB), and for a comment
its parent the same way, to see whether it answers a hiring thread (a failed read leaves it a
comment). An item that does not exist or was deleted is `missing`.

## Card

A story or poll is a `DiscussionForumPosting` named by its title; a comment is a `Comment`
named after its author ("Comment by …"). The description is an excerpt of its text.

Two kinds of item are `JobPosting`s ([Jobs](../api.md#jobs)), posted at their time (`start`):

- a job item ("9 Mothers (YC P26) Is Hiring in Austin, TX"), named by its title; its employer is
  the name before " (YC …", and its `link` is where to apply;
- a top-level comment of a monthly "Ask HN: Who is hiring?" thread (a story by `whoishiring`)
  whose first line is a header in the thread's convention, parts separated by `|`, `—` or `–`
  (an ASCII `-` is not a separator: it is part of words such as "Full-time")
  ("FUTO | https://futo.tech/ | Austin, TX | REMOTE(Worldwide) or ONSITE | Full Time"). It is
  named by that line; its employer is the first part (without its parentheses); a part saying
  REMOTE makes it remote, with the regions in its parentheses ("REMOTE (US)"); a part naming
  full time, part time, contract or intern gives its employment type; one applies through the
  comment's page. The free-text place is not placed on the map.

| Fact | Meaning |
| --- | --- |
| `author` | The author's user name |
| `item_type` | `story`, `comment`, `job`, `poll` or `pollopt` |
| `published` | RFC 3339 |
| `score`, `comment_count` | For a story |
| `link` | The link a story discusses (another object, not this one) |
| `parent` | For a comment, the item it answers |
| `page_url` | Its page on Hacker News |
| `hiring_organization`, `employment_type`, `remote`, `remote_regions`, `date_posted`, `apply_url` | A job posting's ([Jobs](../api.md#jobs)) |

## Examples

- `https://news.ycombinator.com/item?id=8863`
- `https://hnsignals.com/signal/2921983` (a comment, by its hnsignals page)
- `https://news.ycombinator.com/item?id=49926618` (a job posting in the October 2026 hiring
  thread)
