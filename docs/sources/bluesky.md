# Bluesky

Bluesky posts and profiles, read from the public AppView (`public.api.bsky.app`). No key; one
request per second. Posts are social content: the card quotes at most 200 characters, refreshes
within the hour and is removed once the post is gone.

## Links

A post is named by its author and record key, a profile by its author. The address by DID is the
root: `https://bsky.app/profile/{did}/post/{rkey}` and `https://bsky.app/profile/{did}`. The
post's at-URI, `at://{did}/app.bsky.feed.post/{rkey}`, is an alias of it, and so is an address by
handle (`https://bsky.app/profile/{handle}/post/{rkey}`) after its first fetch: handles change,
DIDs do not.

## What it reads

- A handle's DID: `GET …/xrpc/com.atproto.identity.resolveHandle?handle={handle}`.
- A post: `GET …/xrpc/app.bsky.feed.getPostThread?uri={at-URI}&depth=0&parentHeight=0`.
- A profile: `GET …/xrpc/app.bsky.actor.getProfile?actor={did}`.

A post or actor that does not exist (or is not shown publicly) is `missing`.

## Card

A post is a `SocialMediaPosting` named by the start of its text, with the first attached image
(else the author's avatar). A profile is a `Person` named by its display name, with its avatar
and the start of its description.

An account with the `!no-unauthenticated` self-label (it asks apps not to show its content to
logged-out viewers, and every Pinakes card is public) gets cards without its text, images or
name: a post is a `SocialMediaPosting` named "Post on bsky.app" with only `at_uri`, a profile a
`Person` named "Profile on bsky.app" with only `did`, both with the `opted_out` fact. They are
never found by search (`q=`) and their change history keeps no text. The links still resolve and
group, and their references count.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | For a post, the author's handle and display name |
| `published` | RFC 3339 |
| `likes`, `reposts`, `replies` | For a post |
| `at_uri` | The post's at-URI |
| `handle`, `did`, `followers`, `posts` | For a profile |

## Examples

- `https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r`
- `https://bsky.app/profile/bsky.app` (a profile)
