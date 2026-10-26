# TikTok

TikTok videos and profiles, from TikTok's oEmbed endpoint. No key and no account. Videos are
social content (their title is the author's caption): the card quotes at most 200 characters,
refreshes within the hour and is removed once the video is gone.

## Links

A profile is named by its handle, a video by its author and id:
`https://www.tiktok.com/@{handle}` and `https://www.tiktok.com/@{handle}/video/{id}` are the
roots. The hosts without `www.` or with `m.`, and a capitalised handle, are aliases of them.
Short links (`vm.tiktok.com/…`, `/t/…`) name nothing without a fetch and are not read.

## What it reads

- `GET https://www.tiktok.com/oembed?url={root}`, the documented oEmbed endpoint, for a video
  and for a profile alike.

To a request without a browser TikTok names a video by its id alone (`embed_product_id`), so
the card is "Video on tiktok.com" with the author the link spells; when the answer names the
video (`title`, `author_name`, `author_unique_id`, `thumbnail_url`), the card has them. An
answer for another video is an error. A profile's answer names the profile.

A request TikTok refuses (a challenge, 401 or 403) gives the card of the link's shape alone,
"Video on tiktok.com" or "Profile on tiktok.com", with the `blocked` fact: the link still
resolves, groups and counts.

## Card

A video is a `VideoObject` named by its title, with its thumbnail. A profile is a `Person`
named by its display name.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | A video's author's handle and display name |
| `handle` | A profile's handle |
| `blocked` | `true` when TikTok refused the request |

## Examples

- `https://www.tiktok.com/@scout2015/video/6718335390845095173`
- `https://www.tiktok.com/@tiktok` (a profile)
