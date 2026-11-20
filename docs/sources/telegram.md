# Telegram

Telegram public channels and their posts, from t.me's public pages. No key and no account.
Posts are social content: the card quotes at most 200 characters, refreshes within the hour and
is removed once the post is gone.

## Links

A channel (or any public name) is named by its name, a post by its channel and number:
`https://t.me/{name}` and `https://t.me/{name}/{n}` are the roots. telegram.me, the preview
address (`https://t.me/s/{name}[/{n}]`) and a capitalised name are aliases of them. Invite
links (`/+…`, `/joinchat/…`) and private channels (`/c/…`) are not read.

## What it reads

- A post: `GET https://t.me/{name}/{n}?embed=1`, the page Telegram's post widget loads. A
  widget that says "Post not found" is `missing`.
- A channel: `GET https://t.me/{name}`, its Open Graph tags. A name without a public page (a
  user, a private channel, no such name) answers "Telegram: Contact @name": the plain card,
  "Profile on t.me", with the name alone.

A request t.me refuses (a challenge, 401 or 403) gives the card of the link's shape alone with
the `blocked` fact: the link still resolves, groups and counts. So does a page t.me withholds
where the request came from (451): it is not `missing`.

## Card

A post is a `SocialMediaPosting` named by the start of its text (else "Post by {channel}"),
with its text as description and its photo or video still. A channel is a `Person` named by
its title, with its description and its photo.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | For a post, the channel's name and title |
| `published` | RFC 3339 |
| `views` | As the widget shows it (`2.82M`) |
| `handle` | For a channel |
| `blocked` | `true` when t.me refused the request |

## Examples

- `https://t.me/telegram/300`
- `https://t.me/telegram` (a channel)
