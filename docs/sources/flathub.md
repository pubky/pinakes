# Flathub

Pinakes reads desktop apps on Flathub from Flathub's v2 API (the app's AppStream data), by app ID.
No key is needed.

| | |
| --- | --- |
| Source | `flathub`, official source |
| Links | `flathub.org/[{language}/]apps/[details/]{id}` |
| Fetches | `GET https://flathub.org/api/v2/appstream/{id}` |
| Credentials | None |
| Rate | 1 request per second on `flathub.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareApplication` |
| Keys | None of its own. The `app_stores` also-on source records `flathub:{id}` |
| Credit | None |
| Switch | `[sources.flathub] enabled`, on by default |

## Links

Every address of one app is one resource. The root is `https://flathub.org/apps/{id}`, and these
links are [aliases](../api.md#aliases) of it:

- `https://flathub.org/apps/details/{id}`;
- either path under a site language, such as `/de/apps/{id}` or `/pt-BR/apps/{id}`;
- any of them on `www.flathub.org`, or with a trailing slash.

An app ID is a reverse-DNS name with at least three parts (`org.signal.Signal`), made of letters,
digits, `_` and `-`. It keeps its case.

Not read here: collections, pages under an app (its reviews) and every other page on the site.

## What it fetches

One request per read: `GET https://flathub.org/api/v2/appstream/{id}`. An app Flathub does not know
(404) is `missing`. An answer for another app, or any other status, is an error and the read is
retried.

## Card

The type is `SoftwareApplication`.

- `site` is "Flathub", `name` the app's name, `description` its summary and `image` its icon.

| Fact | Meaning |
| --- | --- |
| `developer` | The developer's name |
| `category` | The first of its AppStream categories, for example "Network" |
| `website` | The project's homepage |
| `license` | The project's licence, for example `AGPL-3.0-only` |
| `source_code` | The project's source repository |
| `page_url` | `https://flathub.org/apps/{id}` |

## Keys and groups

The source records no keys. The `app_stores` also-on source (the `apps` lookup family) records the
app's own store key, `flathub:{id}`, and finds the app's other stores through Wikidata, so the app's
store links join one [group](../api.md#groups). It also compares the card's name and `website` with
other store cards, as a hint ([possibly the same app](../api.md#possibly-the-same-app)).

## Credit

The card shows no credit line. `license` names the project's own licence, as its AppStream data
states it.

## Turn it off

```toml
[sources.flathub]
enabled = false
```

Flathub links then get ordinary web cards.

## Examples

- `https://flathub.org/apps/org.signal.Signal`
