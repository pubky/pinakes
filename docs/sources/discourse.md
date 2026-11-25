# Discourse

Pinakes reads topics and posts on any Discourse forum from the forum's own JSON. No key is needed.
Pinakes knows a forum by its link shapes, so it tries those links on any host and remembers the
hosts that do not answer as Discourse. Posts are social content.

| | |
| --- | --- |
| Source | `discourse`, official source |
| Links | On any `https` host: `/t/{slug}/{id}`, `/t/{id}`, `/t/{slug}/{id}/{n}`, `/t/{id}/{n}`, `/p/{id}` |
| Fetches | `GET https://{host}/t/{id}.json`, `GET https://{host}/t/{id}/{n}.json` or `GET https://{host}/posts/{id}.json` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`), per forum |
| Refresh | Within the hour (social content) |
| Card types | `DiscussionForumPosting`, `Comment`, `Question` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.discourse] enabled`, on by default |

## Links

On any host, these `https` links are read as forum links:

- a topic: `/t/{slug}/{id}` or `/t/{id}`; `/t/{slug}/{id}/1`, its first post, is the topic too;
- a post in a topic: `/t/{slug}/{id}/{n}` or `/t/{id}/{n}`, for a post number `n` above 1;
- a post by its own ID: `/p/{id}`.

IDs and post numbers are positive decimal numbers of at most 10 digits. A slug is not all digits.
Links with a port, and `http` links, are not read.

The forum names the topic's slug, so the root is known only after the first fetch. The root of a
topic is `https://{host}/t/{slug}/{id}`, and of a later post `https://{host}/t/{slug}/{id}/{n}`.
Every other address of the same topic or post becomes an [alias](../api.md#aliases) of its root
after its first fetch: the address without the slug, one with tracking such as `?u={user}`, or
`/p/{id}`.

Not read here: category pages (`/c/…`), user pages and every other forum page.

### Sites that are not Discourse

A host whose answer is not JSON at all is not Discourse. Pinakes remembers that for a day, and its
links get ordinary web cards during that time.

A JSON refusal is about that topic alone, such as a topic in a private category: the link gets a
web card, and the forum's other links are still read.

## What it fetches

Each read makes one request, by what the link names:

| Link | Request |
| --- | --- |
| A topic | `GET https://{host}/t/{id}.json` |
| Post `n` in a topic | `GET https://{host}/t/{id}/{n}.json` |
| A post by its ID | `GET https://{host}/posts/{id}.json` |

The read asks for the body of error answers too. The answers mean:

- A 404, or a 410 for a deleted topic or post, whose JSON `error_type` is `not_found`: the card is
  `missing`.
- A post number the topic's answer does not hold: the card is `missing`.
- A 401, 403, 405, or a 404 or 410 without that `error_type`: the link gets a web card.
- An answer that is not JSON, or a topic without a title or a valid slug: the link gets a web card.
- A 408, 429 or 5xx answer: the read is retried after at least a minute.
- Any other status, or an answer for another topic or post: an error, and the read is retried.

## Card

`site` is the forum's host. `description` is the start of the shown post's text: plain text of at
most 200 characters.

### Topics

A topic is a `DiscussionForumPosting`, named by its title and described by its first post. Its
`image` is the topic's image, when the forum gives one.

A topic its asker marked solved (the Solved plugin's `accepted_answers`) is a `Question`
([questions](../api.md#questions)). It holds the link to its first accepted answer and its number
of replies.

### Posts

A post after the first, read through its topic, is a `Comment` named after its topic, for example
"Comment on “Unspendable keys in descriptors”".

A post read by its own ID (`/p/{id}`) is named after its author, for example
"Post by Pieter Wuille". It is a `DiscussionForumPosting` when it is a topic's first post, else a
`Comment`. Its link becomes an alias of the post's address in its topic.

| Fact | Meaning |
| --- | --- |
| `author` | The user name of the shown post's author |
| `published` | When the shown post was written, as the forum gives it |
| `post_count` | For a topic, the number of posts |
| `tags` | For a topic, its tags |
| `thread` | For a post read through its topic, the topic's root |
| `accepted_answer` | For a solved topic, the link to the accepted answer |
| `answer_count` | For a solved topic, the number of replies (posts after the first) |

## Keys and groups

Discourse topics and posts have no keys and join no [groups](../api.md#groups).

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.discourse]
enabled = false
```

Forum links then get ordinary web cards.

## Examples

- `https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304`
- `https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2` (a reply)
- `https://meta.discourse.org/t/how-to-clear-missing-unread-messages/413333` (a solved topic)
