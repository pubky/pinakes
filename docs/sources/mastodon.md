# Mastodon

Posts on any Mastodon server, read from that server's own API. No key; Mastodon allows 300
requests per 5 minutes per address (one per second here). Posts are social content: the card
quotes at most 200 characters, refreshes within the hour and is removed once the post is gone.

## Links

A link of the shape `https://{server}/@{account}/{id}` or
`https://{server}/users/{account}/statuses/{id}` (with a numeric post id) is read from
`https://{server}/api/v1/statuses/{id}`. A post is keyed on its ActivityPub id, which the API
names: every address of it on its own server becomes an alias of that id after its first fetch.
A server's copy of another server's post (`https://{other}/@{account}@{server}/{id}`), or any
answer naming an id on another host, keeps its own card, with the id as the `activitypub_id`
fact: only the id's own server can say that two links are one post.

A server that does not answer as Mastodon (another program, or one that refuses unauthenticated
reads) is remembered for a day, and its links get ordinary web cards (which read ActivityPub
where the server offers it).

## What it reads

`GET https://{server}/api/v1/statuses/{id}`: the post's text, content warning, author, time,
counts, attachments, address and ActivityPub id. A post the server says does not exist is
`missing`.

## Card

A `SocialMediaPosting` named by the start of its text (or its content warning, which stands in for
the text it hides), with its first attachment's preview unless it is marked sensitive (else the
author's avatar).

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | The author's account (`name` or `name@server`) and display name |
| `published` | RFC 3339 |
| `replies`, `reblogs`, `favourites` | Its counts |
| `content_warning` | The post's content warning, if it has one |
| `activitypub_id` | Its ActivityPub id, the root of every address of it |
| `page_url` | Its page, when it is on the server the link names |

## Examples

- `https://mastodon.social/@Gargron/1`
