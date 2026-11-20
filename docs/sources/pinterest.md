# Pinterest

Pinterest pins and profiles. No key and no account: a pin comes from Pinterest's oEmbed
endpoint, a profile from its page's Open Graph tags. Pins are social content: the card quotes at
most 200 characters, refreshes within the hour and is removed once the pin is gone.

## Links

A pin is named by its id, a profile by its name: `https://www.pinterest.com/pin/{id}/` and
`https://www.pinterest.com/{name}/` are the roots. Pinterest's country sites (pinterest.co.uk,
pinterest.de, …), the hosts without `www.`, a pin's slug (`/pin/{slug}--{id}/`) and a
capitalised name are aliases of them. Boards and `pin.it` short links are not read.

## What it reads

- A pin: `GET https://www.pinterest.com/oembed.json?url={root}`, the documented oEmbed
  endpoint. A 400 or 404 (Pinterest answers "Url was not found") is `missing`.
- A profile: `GET https://www.pinterest.com/{name}/`, its Open Graph tags and JSON-LD. A page
  that is no profile's (no such name) is `missing`.

A request Pinterest refuses (a challenge, 401 or 403) gives the card of the link's shape alone,
"Post on pinterest.com" or "Profile on pinterest.com", with the `blocked` fact: the link still
resolves, groups and counts. So does a profile page Pinterest withholds where the request came
from (451): it is not `missing`.

## Card

A pin is a `SocialMediaPosting` named by its title (else "Post by {pinner}"), with its
thumbnail. A profile is a `Person` named by its display name, with its bio and its avatar.

| Fact | Meaning |
| --- | --- |
| `author`, `author_name` | For a pin, the pinner's name and display name |
| `handle` | For a profile |
| `blocked` | `true` when Pinterest refused the request |

## Examples

- `https://www.pinterest.com/pin/99360735500167749/`
- `https://www.pinterest.com/pinterest/` (a profile)
