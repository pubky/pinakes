# Hacker News

Pinakes reads Hacker News stories, comments, jobs and polls from the official Firebase API, by
item ID. No key is needed. Items are social content.

| | |
| --- | --- |
| Source | `hacker_news`, official source |
| Links | `news.ycombinator.com/item?id={id}`, `hacker-news.firebaseio.com/v0/item/{id}.json` |
| Fetches | `GET https://hacker-news.firebaseio.com/v0/item/{id}.json`; for a comment, its parent the same way |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Within the hour (social content) |
| Card types | `DiscussionForumPosting`, `Comment`, `JobPosting` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.hacker_news] enabled`, on by default |

## Links

Every address of one item is one resource. The root is
`https://news.ycombinator.com/item?id={id}`, and these links are [aliases](../api.md#aliases) of
it:

- the same item page with other query parameters, such as `&p=2`;
- `https://hacker-news.firebaseio.com/v0/item/{id}.json`, the API's address.

An item ID is a positive decimal number of at most 12 digits.

Not read here: user pages (`news.ycombinator.com/user?id=…`), the front page, an `item` link
whose `id` is not a number, and other sites' pages that name an item by its ID (such as
`https://hnsignals.com/signal/{id}`). Those get web cards of their own.

## What it fetches

Each read makes one request: `GET https://hacker-news.firebaseio.com/v0/item/{id}.json`. An item
the API answers with `null`, or one marked deleted, is `missing`. Any other status than 200, or an
answer for another item ID, is an error, and the read is retried.

For a comment, Pinakes then reads its parent the same way, to see whether the comment is a
top-level answer in a hiring thread (see [Job postings](#job-postings)). If that second read fails,
the card stays a comment.

## Card

The type follows the item's type:

- A story, poll or other non-comment item is a `DiscussionForumPosting`, named by its title.
- A comment is a `Comment`, named after its author ("Comment by norvig").
- A job item, and a hiring comment, is a `JobPosting`.

`description` is an excerpt of the item's text: plain text, at most 200 characters.

### Job postings

Two kinds of item are `JobPosting`s ([jobs](../api.md#jobs)). Both are posted at the item's time,
which the card holds as `start`.

A job item, such as "9 Mothers (YC P26) Is Hiring in Austin, TX", is named by its title. Its
employer is the part of the title before " (YC ". Its `apply_url` is the item's link.

A hiring comment is a top-level comment of a monthly "Ask HN: Who is hiring?" thread (a story by
`whoishiring` whose title starts with that text). It is a job posting only when its first line is a
header in the thread's convention, for example:

```text
FUTO | https://futo.tech/ | Austin, TX | REMOTE(Worldwide) or ONSITE | Full Time
```

The header has at least two parts, separated by `|`, `—` or `–`, and at most 300 characters. An
ASCII `-` is not a separator, because it is part of words such as "Full-time". From the header:

- The card is named by the header line (at most 200 characters).
- The employer is the first part, without its parenthesized asides.
- A part saying REMOTE makes the posting remote, with the regions in its parentheses, such as
  "REMOTE (US)".
- A part naming full time, part time, contract, intern or temporary gives the employment type.
- `apply_url` is the comment's page.

A free-text place in the header is not placed on the map. A comment without such a header stays a
`Comment`.

| Fact | Meaning |
| --- | --- |
| `author` | The author's user name |
| `item_type` | The API's item type: `story`, `comment`, `job`, `poll` or `pollopt` |
| `published` | When the item was posted, RFC 3339 |
| `score` | A story's points |
| `comment_count` | A story's number of comments |
| `link` | The link a story points at (another object, not this one) |
| `parent` | For a comment, the page of the item it answers |
| `page_url` | The item's page on Hacker News |
| `hiring_organization` | A job posting's employer |
| `employment_type` | A job posting's employment type, such as `FULL_TIME` |
| `remote` | `true` for a remote job posting |
| `remote_regions` | The regions a remote job posting names |
| `date_posted` | When a job posting was posted |
| `apply_url` | Where to apply: a job item's link, or a hiring comment's page |

## Keys and groups

Hacker News items have no keys and join no [groups](../api.md#groups). A story's `link` is a fact
only: it names another resource, which gets a card of its own.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.hacker_news]
enabled = false
```

Hacker News links then get ordinary web cards.

## Examples

- `https://news.ycombinator.com/item?id=8863`
- `https://news.ycombinator.com/item?id=2921983` (a comment)
- `https://news.ycombinator.com/item?id=49926618` (a job posting in the October 2026 hiring
  thread)
