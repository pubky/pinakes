# Pinterest

Pinakes reads Pinterest pins from Pinterest's oEmbed endpoint and profiles from their pages' Open
Graph tags and JSON-LD. No key or account is needed. Pins and profiles are social content.

| | |
| --- | --- |
| Source | `pinterest`, official source |
| Links | `pinterest.com/pin/{id}/`, `pinterest.com/pin/{slug}--{id}/`, `pinterest.com/{name}/`; the same on Pinterest's country sites |
| Fetches | `GET https://www.pinterest.com/oembed.json?url={pin}` for a pin, `GET https://www.pinterest.com/{name}/` for a profile |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.pinterest.com` |
| Refresh | Within the hour (social content) |
| Card types | `SocialMediaPosting` (pin), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes: 120 characters of a pin's title, 200 of a bio, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.pinterest] enabled`, on by default |

## Links

The roots are:

- a pin: `https://www.pinterest.com/pin/{id}/`;
- a profile: `https://www.pinterest.com/{name}/`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `pinterest.com` and on Pinterest's country sites, with or without `www.`:
  `pinterest.at`, `.ca`, `.ch`, `.cl`, `.co.kr`, `.co.uk`, `.com.au`, `.com.mx`, `.de`, `.dk`,
  `.es`, `.fr`, `.ie`, `.it`, `.jp`, `.nl`, `.nz`, `.ph`, `.pt`, `.ru` and `.se`;
- the same addresses with `http`;
- a pin's address with its slug, `/pin/{slug}--{id}/`;
- a name with capitals (roots are lower case), an address without the trailing slash or with query
  parameters.

A pin ID is a number of 8 to 20 digits. A name is 3 to 30 letters, digits and underscores.

Not read here: boards (`/{name}/{board}/`), `pin.it` short links, search pages and Pinterest's
other own pages (such as `/ideas` and `/today`).

## What it fetches

### Pins

One request to Pinterest's oEmbed endpoint, with the pin's root as `url`. A 400 or 404 is
`missing` (Pinterest answers a pin it doesn't know with a 400). An answer that is not a JSON object
and any other status are errors, and the read is retried.

### Profiles

One request for the profile's page. A 404 or 410 is `missing`. Any status that is neither listed
here nor a refusal is an error, and the read is retried.

Pinterest answers its generic page, with status 200, for a name it doesn't know. That page does not
say that the profile is gone: it gives the plain card (below), not `missing`.

A 429 to either request waits at least the time its `Retry-After` header names (up to an hour), or
5 minutes when it names none. A 408 or a server error to either request waits at least a minute.

### Refused requests

Pinterest refuses a request with a sign-in redirect, a challenge page, or a 401 or 403; for a
profile's page, a 451 counts too. A link never read before then gets the card of its shape alone,
"Post on pinterest.com" or "Profile on pinterest.com", with the description "Blocked by the site."
and the `blocked` fact. The link still resolves, groups and counts. A refusal on a refresh keeps
the card read before, and the link is asked again in an hour.

## Card

### Pins

A pin is a `SocialMediaPosting`.

- `name` is the first 120 characters of the pin's title, or "Post by {pinner}" for a pin without
  one.
- `description` is empty.
- `image` is the pin's thumbnail.

### Profiles

A profile is a `Person` named by its display name. `description` is the first 200 characters of the
bio, without e-mail addresses. `image` is the avatar.

The card takes these only from the profile's own page: the address the fetch ended at must name the
link's profile, and the page must describe a person. Any other page gives the plain card, "Profile
on pinterest.com" with the `handle` fact. A stored card that says more stays until a second read,
an hour later by default (`limits.missing_ttl_secs`, at least an hour), answers the same; then the
plain card replaces it.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a pin: the pinner's name, in lower case |
| `author_name` | For a pin: the pinner's display name |
| `handle` | For a profile: its name, in lower case |
| `blocked` | `true` when Pinterest refused the request |

## Keys and groups

Pinterest cards have no keys. Names and pin IDs join nothing.

## Credit

The card credits nothing. A pin's pinner is in `author` and `author_name`.

## Turn it off

```toml
[sources.pinterest]
enabled = false
```

Pinterest links then get ordinary web cards.

## Examples

- `https://www.pinterest.com/pin/99360735500167749/`
- `https://www.pinterest.com/pinterest/` (a profile)
