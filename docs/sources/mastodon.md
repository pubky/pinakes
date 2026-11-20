# Mastodon

Posts on any Mastodon server, read from that server's own API. No key; Mastodon allows 300
requests per 5 minutes per address. Each server is asked at the default pace,
`limits.host_requests_per_second` (one request a second unless the operator changes it). Posts
are social content: the card quotes at most 200 characters, refreshes within the hour and is
removed once the post is gone.

## Links

A link of the shape `https://{server}/@{account}/{id}` or
`https://{server}/users/{account}/statuses/{id}` (with a numeric post id) is read from
`https://{server}/api/v1/statuses/{id}`. The post must belong to the account the
link names: the answer's `account.acct`, compared without case. `@{account}@{server}` may name an
account of `{server}` itself; the `/users/…` form names only the server's own accounts. A link
that names another account gets an ordinary web card. A post is keyed on its ActivityPub id, which the API
names: every address of it on its own server becomes an alias of that id after its first fetch.
A server's copy of another server's post (`https://{other}/@{account}@{server}/{id}`), or any
answer naming an id on another host, keeps its own card, with the id as the `activitypub_id`
fact: only the id's own server can say that two links are one post.

A server whose answer is not JSON (another program) is remembered for a day, and its links get
ordinary web cards (which read ActivityPub where the server offers it). A JSON refusal (a private
post, a server that refuses unauthenticated reads) gives the link a web card without marking
the server.

## What it reads

`GET https://{server}/api/v1/statuses/{id}`: the post's text, content warning, author, time,
counts, attachments, address and ActivityPub id. A post the server says does not exist (a 404 or
410 in JSON with an `error`) is `missing` when the server answered with a Mastodon post within the
last day. From any other host that answer may be another program's, and the link gets an ordinary
web card.

## Card

A `SocialMediaPosting` named by the start of its text (or its content warning, which stands in for
the text it hides), with its first attachment's preview unless it is marked sensitive (else the
author's avatar).

An author who opted out of search indexing (the account's `indexable: false` or `noindex: true`,
the flag behind the profile's `robots` `noindex` tag) keeps a normal card, the link preview
Mastodon itself shows, with the `opted_out` fact. Its text is never found by search (`q=`), and
its change history keeps no text: what was kept before the opt-out goes, and its change lines
carry only their kind, fields and time. The spellings of the post saved before they became its
aliases, whenever they become one, keep only the line naming the post. A deleted post keeps
nothing of itself: no name, text, image or fact (its content warning included), no history,
and the type every gone post has.

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
