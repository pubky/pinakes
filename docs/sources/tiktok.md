# TikTok

TikTok videos and profiles. No key and no account. A profile is read from TikTok's oEmbed
endpoint; a video is a link rule: its card is the link's root and shape, with the author the
link spells, and nothing is asked (to a request without a browser TikTok names a video by its
id alone). A profile is social content too: it refreshes within the hour, is removed once it is gone, and its bio is published without e-mail addresses.

## Links

A profile is named by its handle, a video by its author and id:
`https://www.tiktok.com/@{handle}` and `https://www.tiktok.com/@{handle}/video/{id}` are the
roots. The hosts without `www.` or with `m.`, and a capitalised handle, are aliases of them.
Short links (`vm.tiktok.com/…`, `/t/…`) name nothing without a fetch and are not read.

## What it reads

- A profile: `GET https://www.tiktok.com/oembed?url={root}`, the documented oEmbed endpoint.
  Its answer names the profile.
- A video: nothing. The card is "Video on tiktok.com" with the author the link spells.

A profile's request TikTok refuses (a challenge, 401 or 403) gives the card of the link's shape
alone, "Profile on tiktok.com", with the `blocked` fact: the link still resolves, groups
and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does.

## Card

A video is a `VideoObject` named by its shape, "Video on tiktok.com". A profile is a `Person`
named by its display name.

| Fact | Meaning |
| --- | --- |
| `author` | A video's author's handle, as the link spells it |
| `handle` | A profile's handle |
| `blocked` | `true` when TikTok refused the request |

## Examples

- `https://www.tiktok.com/@scout2015/video/6718335390845095173`
- `https://www.tiktok.com/@tiktok` (a profile)
