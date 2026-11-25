# Bluesky

Pinakes reads Bluesky posts and profiles from Bluesky's public AppView API, by the account's DID.
No key is needed. Posts and profiles are social content.

| | |
| --- | --- |
| Source | `bluesky`, official source |
| Links | `bsky.app/profile/{actor}`, `bsky.app/profile/{actor}/post/{rkey}`, `at://{actor}`, `at://{actor}/app.bsky.feed.post/{rkey}`; `{actor}` is a DID or a handle |
| Fetches | `GET https://public.api.bsky.app/xrpc/…`: `app.bsky.feed.getPostThread` for a post, `app.bsky.actor.getProfile` for a profile, `com.atproto.identity.resolveHandle` first for a link by handle |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `public.api.bsky.app` |
| Refresh | Within the hour (social content) |
| Card types | `SocialMediaPosting` (post), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.bluesky] enabled`, on by default |

## Links

The root of a post or profile is its address by DID, because handles change and DIDs do not:

- a post: `https://bsky.app/profile/{did}/post/{rkey}`;
- a profile: `https://bsky.app/profile/{did}`.

These links are [aliases](../api.md#aliases) of the root:

- `at://{did}/app.bsky.feed.post/{rkey}` and `at://{did}`, known from the link alone;
- the same addresses on `www.bsky.app` or with `http`;
- every address by handle (`https://bsky.app/profile/{handle}/post/{rkey}`,
  `https://bsky.app/profile/{handle}` and the `at://{handle}…` forms), after its first successful
  read: Pinakes first resolves the handle to its DID.

A DID is `did:plc:…` or `did:web:…`. A handle is a domain name with at least one dot. A record key
is at most 512 letters, digits and `.-_:~`, but not `.` or `..`.

Not read here: lists, feeds, search pages, other record types (such as
`at://{did}/app.bsky.feed.like/…`) and links with a port or user info.

## What it fetches

A link by DID makes one request. A link by handle makes two: it resolves the handle first.

| Request | When |
| --- | --- |
| `GET …/xrpc/com.atproto.identity.resolveHandle?handle={handle}` | The link names a handle |
| `GET …/xrpc/app.bsky.feed.getPostThread?uri={at-URI}&depth=0&parentHeight=0` | The link names a post |
| `GET …/xrpc/app.bsky.actor.getProfile?actor={did}` | The link names a profile |

These answers make a card `missing`:

- a 400 to `resolveHandle` whose `error` is `InvalidRequest`, `NotFound` or `HandleNotFound`;
- a 400 to `getPostThread` whose `error` is `NotFound`, or a thread that Bluesky types
  `#notFoundPost` or `#blockedPost`;
- a 400 to `getProfile` whose `error` is `InvalidRequest`, `NotFound` or `AccountTakedown`.

Any other status, an answer that is not the expected JSON and an answer for another post or account
are errors: the read is retried and a stored card stays. A 408, a 429 or a server error waits at
least a minute.

## Card

### Posts

A post is a `SocialMediaPosting`.

- `name` is the first 120 characters of the text on one line, or "Post by {author}" for a post
  without text.
- `description` is the first 200 characters of the text, with its line breaks.
- `image` is the thumbnail of the first attached image, else the author's avatar.

### Profiles

A profile is a `Person`.

- `name` is the display name, else the handle.
- `description` is the first 200 characters of the bio, without e-mail addresses.
- `image` is the avatar.

### Accounts hidden from logged-out viewers

An account with the `!no-unauthenticated` self-label asks apps not to show its content to
logged-out viewers. Every Pinakes card is public, so the account's cards hold no text, image or
author:

- a post is a `SocialMediaPosting` named "Post on bsky.app", with only the `at_uri` and `opted_out`
  facts;
- a profile is a `Person` named "Profile on bsky.app", with only the `did` and `opted_out` facts.

These cards still resolve and group, and their references count. A text search (`q=`) never finds
them, related results never list them, and their change history keeps no text.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the author's handle |
| `author_name` | For a post: the author's display name |
| `published` | For a post: its creation time, as the record states it (RFC 3339) |
| `likes`, `reposts`, `replies` | For a post: its counts |
| `at_uri` | For a post: its at-URI, by DID |
| `handle` | For a profile: its handle |
| `did` | For a profile: its DID |
| `followers`, `posts` | For a profile: its counts |
| `opted_out` | `true` when the account has the `!no-unauthenticated` self-label |

## Keys and groups

Bluesky cards have no keys. The DID, handle and at-URI are facts only and join nothing.

## Credit

The card credits nothing. A post's author is in `author` and `author_name`.

## Turn it off

```toml
[sources.bluesky]
enabled = false
```

`bsky.app` links then get ordinary web cards, and `at://` links wait until the source is on again.

## Examples

- `https://bsky.app/profile/bsky.app/post/3mxzez2m7wk2r`
- `https://bsky.app/profile/bsky.app` (a profile)
