# Lobsters

Stories on Lobsters, read from each story's own JSON (`/s/{id}.json`). No key; one request per
second. Stories are social content: the card quotes at most 200 characters, refreshes within the
hour and is removed once the story is gone.

## Links

`https://lobste.rs/s/{id}` is the root; the story page with its slug (`/s/{id}/{slug}`) and its
JSON are aliases of it.

## What it reads

`GET https://lobste.rs/s/{id}.json`. A story Lobsters does not know is `missing`.

## Card

A `DiscussionForumPosting` named by its title and described by its text, if it has one.

| Fact | Meaning |
| --- | --- |
| `author` | The submitter |
| `published` | RFC 3339 |
| `score`, `comment_count` | Its score and comments |
| `tags` | E.g. `rust; math` |
| `link` | The link the story discusses (another object, not this one) |
| `page_url` | Its page on Lobsters |

## Examples

- `https://lobste.rs/s/wtthju/a5_v1_0`
