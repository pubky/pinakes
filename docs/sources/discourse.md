# Discourse

Topics and posts on any Discourse forum, read from the forum's own JSON. No key; Discourse's
default limits are 200 requests per minute and 50 per 10 seconds per address (one per second
here). Posts are social content: the card quotes at most 200 characters, refreshes within the
hour and is removed once the post is gone.

## Links

On any host, these are read as forum links:

- a topic: `/t/{slug}/{id}`, `/t/{id}` (and `/t/{slug}/{id}/1`, its first post);
- a post in a topic: `/t/{slug}/{id}/{n}` and `/t/{id}/{n}`;
- a post by its own id: `/p/{id}`.

The forum names the topic's slug, and the address with the slug is the root: every other address
of the topic or post (without the slug, with tracking such as `?u={user}`, or `/p/{id}`) becomes an
alias of it after its first fetch. A site whose answer is not Discourse's JSON is remembered for a
day, and its links get ordinary web cards; a JSON refusal (a topic in a private category) is
about that topic alone.

## What it reads

`GET https://{host}/t/{id}.json`, `…/t/{id}/{n}.json` or `…/posts/{id}.json`. A topic or post the
forum says does not exist (a 404, or a 410 for a deleted one, with the `error_type` `not_found`)
is `missing`.

## Card

A topic is a `DiscussionForumPosting` named by its title and described by the start of its first
post, with its image; a post after the first is a `Comment` ("Comment on “…”"). `site` is the
forum's host. A topic its asker marked solved (the Solved plugin's `accepted_answers`) is a
`Question` ([Questions](../api.md#questions)) with the link to its first accepted answer and its
number of replies.

| Fact | Meaning |
| --- | --- |
| `author` | The post's author |
| `published` | RFC 3339 |
| `post_count`, `tags` | For a topic |
| `thread` | For a post, its topic |
| `accepted_answer`, `answer_count` | For a solved topic: the accepted answer's address and the replies |

## Examples

- `https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304`
- `https://delvingbitcoin.org/t/unspendable-keys-in-descriptors/304/2` (a reply)
- `https://meta.discourse.org/t/how-to-clear-missing-unread-messages/413333` (a solved topic)
