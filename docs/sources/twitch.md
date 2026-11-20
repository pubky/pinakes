# Twitch

Twitch channels, videos and clips, from their pages' Open Graph tags. No key and no account.
Videos and clips are social content (a video's title is its creator's own text): the card quotes
at most 120 characters of the title, refreshes within the hour and is removed once Twitch
answers that the video is gone (404 or 410).

## Links

A channel is named by its login, a video by its id, a clip by its slug:
`https://www.twitch.tv/{login}`, `https://www.twitch.tv/videos/{id}` and
`https://clips.twitch.tv/{slug}` are the roots. m.twitch.tv, the host without `www.`, a
capitalised login, `/{login}/video/{id}` (also `/v/`) and `/{login}/clip/{slug}` are aliases
of them.

## What it reads

- A channel: `GET https://www.twitch.tv/{login}`, its Open Graph tags (`Name - Twitch`). A page that
  is not a profile's (Twitch answers its generic page, with status 200, for a channel that does not
  exist) gives the plain card, "Profile on twitch.tv", with the login: it does not say the channel
  is gone, and a stored card that says more (another name, text, a picture or facts) stays, as last
  read, for two hours from that read (by default; twice `queue.missing_ttl_secs`, at least an hour
  each): a page answered without tags for that long gets the plain card. A 404 or 410 is `missing`.
- A video: `GET https://www.twitch.tv/videos/{id}`, its Open Graph tags
  (`Title - login on Twitch`).
- A clip: `GET https://clips.twitch.tv/{slug}`. Twitch names a clip only to a browser, so the
  card is the plain one, "Video on twitch.tv", with the channel when the link spells it.

A request Twitch refuses (a challenge, 401 or 403) gives the card of the link's shape alone
with the `blocked` fact: the link still resolves, groups and counts. So does a page Twitch
withholds where the request came from (451): it is not `missing`.

## Card

A channel is a `Person` named by its display name, with its bio and its avatar. A video or a
clip is a `VideoObject` named by its title, with its still.

| Fact | Meaning |
| --- | --- |
| `author` | A video's or clip's channel |
| `category` | A video's category, as its page names it |
| `handle` | A channel's login |
| `blocked` | `true` when Twitch refused the request |

## Examples

- `https://www.twitch.tv/twitch` (a channel)
- `https://www.twitch.tv/videos/2896433713`
