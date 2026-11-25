# Snap Store

Pinakes reads Linux apps on the Snap Store from the snapcraft.io v2 info API, by snap name. No key is
needed. Every request carries the header the API asks for, `Snap-Device-Series: 16`.

| | |
| --- | --- |
| Source | `snap_store`, official source |
| Links | `snapcraft.io/{name}`, `snapcraft.io/install/{name}/{distribution}` |
| Fetches | `GET https://api.snapcraft.io/v2/snaps/info/{name}?fields=…` |
| Credentials | None |
| Rate | 1 request per second on `api.snapcraft.io` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareApplication` |
| Keys | None of its own. The `app_stores` also-on source records `snap:{name}` |
| Credit | None |
| Switch | `[sources.snap_store] enabled`, on by default |

## Links

Every address of one snap is one resource. The root is `https://snapcraft.io/{name}`, and these
links are [aliases](../api.md#aliases) of it:

- `https://snapcraft.io/install/{name}/{distribution}`;
- either on `www.snapcraft.io`, or with a trailing slash or a query.

A snap name is at most 40 lower-case letters, digits and `-`. It holds at least one letter and does
not start or end with `-`.

Not read here: the site's own pages at its root, `about`, `account`, `blog`, `docs`, `first-snap`,
`iot`, `login`, `logout`, `publisher`, `search`, `snaps`, `store` and `tutorials`.

## What it fetches

One request per read:
`GET https://api.snapcraft.io/v2/snaps/info/{name}?fields=title,summary,publisher,media,website,license,categories`,
with the header `Snap-Device-Series: 16`. Because the request carries a header, a redirect is
refused, not followed.

A name the store does not know (404) gets an ordinary web card, because the link may be another page
of the site. That card is `missing` when the page is gone too. An answer for another snap, or any
other status, is an error and the read is retried.

## Card

The type is `SoftwareApplication`.

- `site` is "Snap Store", `name` the snap's title (its name when it has no title), `description` its
  summary and `image` its icon.

| Fact | Meaning |
| --- | --- |
| `developer` | The publisher's display name: who packages the snap, not always the app's developer |
| `category` | The snap's featured category, else its first |
| `website` | The website the publisher gives |
| `license` | The snap's licence, for example `AGPL-3.0-only` |
| `page_url` | `https://snapcraft.io/{name}` |

## Keys and groups

The source records no keys. The `app_stores` also-on source (the `apps` lookup family) records the
app's own store key, `snap:{name}`, and finds the app's other stores through Wikidata, so the app's
store links join one [group](../api.md#groups). It also compares the card's name and `website` with
other store cards, as a hint ([possibly the same app](../api.md#possibly-the-same-app)).

## Credit

The card shows no credit line. `license` names the snap's own licence, as its record states it.

## Turn it off

```toml
[sources.snap_store]
enabled = false
```

Snap Store links then get ordinary web cards.

## Examples

- `https://snapcraft.io/signal-desktop`
