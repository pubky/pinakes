# Twitch

Pinakes reads Twitch channels and videos from their pages' Open Graph tags and JSON-LD. A clip's
card comes from the link alone and nothing is fetched, because Twitch names a clip only to a
browser. No key or account is needed. Channels, videos and clips are social content.

| | |
| --- | --- |
| Source | `twitch`, official source |
| Links | `twitch.tv/{login}`, `twitch.tv/videos/{id}`, `twitch.tv/{login}/video/{id}`, `twitch.tv/{login}/v/{id}`, `twitch.tv/{login}/clip/{slug}`, `clips.twitch.tv/{slug}` |
| Fetches | `GET https://www.twitch.tv/{login}` for a channel, `GET https://www.twitch.tv/videos/{id}` for a video; nothing for a clip |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.twitch.tv` |
| Refresh | Within the hour (social content); a clip's refresh makes no request |
| Card types | `Person` (channel), `VideoObject` (video, clip) |
| Keys | None (facts only) |
| Social content | Yes: 120 characters of a video's title, 200 of a bio; a channel or video is purged when Twitch answers 404 or 410 ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.twitch] enabled`, on by default |

## Links

The roots are:

- a channel: `https://www.twitch.tv/{login}`;
- a video: `https://www.twitch.tv/videos/{id}`;
- a clip: `https://clips.twitch.tv/{slug}`.

These links are [aliases](../api.md#aliases) of the root:

- a channel or video address on `twitch.tv` or `m.twitch.tv`, and any address with `http`;
- `/{login}/video/{id}` and `/{login}/v/{id}`, aliases of the video;
- `/{login}/clip/{slug}`, an alias of the clip;
- a login with capitals (roots are lower case), an address with a trailing slash or query
  parameters.

A login is 3 to 25 letters, digits and underscores. Twitch's own path names, such as `directory`,
`settings` and `videos`, are never a login. A video ID is a number of at most 20 digits. A
clip slug is 8 to 120 letters, digits, `-` and `_`.

Not read here: `player.twitch.tv` links, a channel's sub-pages (such as `/{login}/about`) and
Twitch's own pages (such as `/directory/…`).

## What it fetches

A channel's or video's read makes one request, for the root's page. A 404 or 410 is `missing`. A
429 waits at least the time Twitch names (up to an hour), or 5 minutes when it names none. A 408 or
a server error waits at least a minute. Any status that is neither listed here nor a refusal is an
error, and the read is retried.

Twitch answers one generic page, with status 200, for a channel that does not exist and for a page
it names only to a browser. That page does not say that the object is gone: it gives the plain card
(below), not `missing`.

A clip makes no request.

### Refused requests

Twitch refuses a request with a sign-in redirect, a challenge page, or a 401, 403 or 451. A link
never read before then gets the card of its shape alone, "Profile on twitch.tv" or "Video on
twitch.tv", with the description "Blocked by the site." and the `blocked` fact. The link still
resolves, groups and counts. A refusal on a refresh keeps the card read before. After a refusal the
link is asked again after a day, then after 2, 4 and 7 more days, and every 7 days while one refusal
follows another ([refusals](../operations.md#failures-and-retries)).

## Card

### Channels

A channel is a `Person` named by its display name. `description` is the first 200 characters of its
bio, without e-mail addresses. `image` is its avatar.

The card takes these only from the channel's own page: the address the fetch ended at must name the
channel, and the page must describe a person. Any other page gives the plain card, "Profile on
twitch.tv" with the `handle` fact. A stored card that says more stays until a second read, an hour
later by default (`limits.missing_ttl_secs`, at least an hour), answers the same; then the plain
card replaces it.

### Videos

A video is a `VideoObject`. Its page's title gives the title and the channel
(`Title - login on Twitch`).

- `name` is the first 120 characters of the title.
- `description` is empty.
- `image` is the video's still.

Any other page gives the plain card, "Video on twitch.tv":

- When the fetch ended at another address, a stored card that says more stays until a second read
  answers the same, as for a channel.
- When the video's own page does not describe a video, the plain card replaces the stored one at
  once.

### Clips

A clip is a `VideoObject` named "Video on twitch.tv". It has no text or image. It has an `author`
only when the link that is read spells the channel, which the root `https://clips.twitch.tv/{slug}`
does not.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a video or clip: its channel, in lower case |
| `category` | For a video: its category, as its page names it |
| `handle` | For a channel: its login, in lower case |
| `blocked` | `true` when Twitch refused the request |

## Keys and groups

Twitch cards have no keys. Logins, video IDs and clip slugs join nothing.

## Credit

The card credits nothing. A video's or clip's channel is in `author`.

## Turn it off

```toml
[sources.twitch]
enabled = false
```

Twitch links then get ordinary web cards.

## Examples

- `https://www.twitch.tv/twitch` (a channel)
- `https://www.twitch.tv/videos/2896433713`
