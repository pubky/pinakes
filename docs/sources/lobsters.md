# Lobsters

Pinakes reads stories on Lobsters from each story's own JSON (`/s/{id}.json`). No key is needed.
Stories are social content.

| | |
| --- | --- |
| Source | `lobsters`, official source |
| Links | `lobste.rs/s/{id}`, `lobste.rs/s/{id}/{slug}`, `lobste.rs/s/{id}.json` |
| Fetches | `GET https://lobste.rs/s/{id}.json` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Within the hour (social content) |
| Card types | `DiscussionForumPosting` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.lobsters] enabled`, on by default |

## Links

Every address of one story is one resource. The root is `https://lobste.rs/s/{id}`, and these
links are [aliases](../api.md#aliases) of it:

- the story page with its slug, `https://lobste.rs/s/{id}/{slug}`;
- the story's JSON, `https://lobste.rs/s/{id}.json`;
- the same links on `www.lobste.rs`.

A story ID is 3 to 12 lower-case letters and digits (Lobsters gives six). A link with upper-case
letters in the ID is not read.

Not read here: tag pages (`/t/…`), the front page and every other Lobsters page.

## What it fetches

Each read makes one request: `GET https://lobste.rs/s/{id}.json`. A story Lobsters does not know
(404) is `missing`. A 429 is retried after at least the time its `Retry-After` header names (up to
an hour), or 5 minutes when it names none. A 408 or 5xx answer is retried after at least a minute.
Any other status, an answer for another story, or a story without a title is an error, and the read
is retried.

## Card

The card is a `DiscussionForumPosting`, named by the story's title. `description` is the story's
own text, if it has one, as plain text cut to 200 characters.

| Fact | Meaning |
| --- | --- |
| `author` | The submitter's user name |
| `published` | When the story was submitted, as Lobsters gives it |
| `score` | The story's score |
| `comment_count` | The number of comments |
| `tags` | The story's tags, for example `compsci; math; rust; visualization` |
| `link` | The link the story points at (another object, not this one) |
| `page_url` | The story's page on Lobsters |

## Keys and groups

Lobsters stories have no keys and join no [groups](../api.md#groups). The story's `link` is a fact
only: it names another resource, which gets a card of its own.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.lobsters]
enabled = false
```

Lobsters links then get ordinary web cards.

## Examples

- `https://lobste.rs/s/wtthju/a5_v1_0`
