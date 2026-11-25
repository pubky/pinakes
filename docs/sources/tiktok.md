# TikTok

Pinakes reads TikTok profiles from TikTok's oEmbed endpoint. A video's card comes from the link
alone and nothing is fetched, because the oEmbed answer for a video names no more than the link
does. No key or account is needed. Videos and profiles are social content.

| | |
| --- | --- |
| Source | `tiktok`, official source |
| Links | `tiktok.com/@{handle}`, `tiktok.com/@{handle}/video/{id}` |
| Fetches | `GET https://www.tiktok.com/oembed?url={profile}` for a profile; nothing for a video |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.tiktok.com` |
| Refresh | Within the hour (social content); a video's refresh makes no request. A link the site refuses: after 1 day, doubling to every 7 days ([refusals](../operations.md#failures-and-retries)) |
| Card types | `VideoObject` (video), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes; the cards quote no text. A profile is purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.tiktok] enabled`, on by default |

## Links

The roots are:

- a video: `https://www.tiktok.com/@{handle}/video/{id}`;
- a profile: `https://www.tiktok.com/@{handle}`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `tiktok.com` and `m.tiktok.com`, or with `http`;
- a handle with capitals (roots are lower case), an address with a trailing slash or query
  parameters (such as `?lang=en`).

A handle is 2 to 24 letters, digits, `_` and `.`. A video ID is a number of at most 20 digits.

Not read here: short links (`vm.tiktok.com/…`, `/t/…`), which name nothing without a fetch, and a
profile's sub-pages (such as `/@{handle}/live`).

## What it fetches

A profile's read makes one request to TikTok's oEmbed endpoint, with the profile's root as `url`.

- A 400 or 404 is `missing`.
- An answer that is not a JSON object and any other status are errors, and the read is retried.
- A 429 waits at least the time TikTok names (up to an hour), or 5 minutes when it names none.
- A 408 or a server error waits at least a minute.

A video makes no request.

### Refused requests

TikTok refuses a request with a sign-in redirect, a challenge page, or a 401 or 403. A profile never
read before then gets the card of its shape alone, "Profile on tiktok.com", with the description
"Blocked by the site." and the `blocked` fact. The link still resolves, groups and counts. A refusal
on a refresh keeps the card read before. After a refusal the link is asked again after a day, then
after 2, 4 and 7 more days, and every 7 days while one refusal follows another
([refusals](../operations.md#failures-and-retries)).

## Card

A video is a `VideoObject` named "Video on tiktok.com", with the author the link spells. It has no
text or image.

A profile is a `Person` named by its display name. The answer's `author_url` must name the link's
handle. Otherwise the card is the plain one, "Profile on tiktok.com" with the `handle` fact, and it
replaces a stored card at once. A profile card has no description or image.

| Fact | Meaning |
| --- | --- |
| `author` | For a video: the author's handle, as the link spells it, in lower case |
| `handle` | For a profile: its handle, in lower case |
| `blocked` | `true` when TikTok refused the request |

## Keys and groups

TikTok cards have no keys. Handles and video IDs join nothing.

## Credit

The card credits nothing. A video's author is in `author`.

## Turn it off

```toml
[sources.tiktok]
enabled = false
```

TikTok links then get ordinary web cards.

## Examples

- `https://www.tiktok.com/@scout2015/video/6718335390845095173`
- `https://www.tiktok.com/@tiktok` (a profile)
