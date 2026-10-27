# Flathub

Desktop apps on Flathub, read from Flathub's v2 API (the app's AppStream data) by app ID. No
key, no account. The metadata is each app's own AppStream file, under the licence its project
states for it.

## Links

`https://flathub.org/apps/{id}` and every Flathub address of the app are one resource:
`/apps/{id}`, `/apps/details/{id}`, either under a site language (`/de/apps/{id}`,
`/pt-BR/apps/{id}`), and on `www.flathub.org`. An app ID is a reverse-DNS name with at least three
parts (`org.signal.Signal`) and keeps its case.

## What it reads

`GET https://flathub.org/api/v2/appstream/{id}`. An app Flathub does not know (404, "App not
found") is `missing`.

## Card

`SoftwareApplication`, with `site` "Flathub", `name` the app's name, `description` its summary and
`image` its icon.

| Fact | Meaning |
| --- | --- |
| `developer` | The developer's name |
| `category` | The first of its AppStream categories, e.g. "Network" |
| `website` | The project's homepage |
| `license` | The project's licence (SPDX) |
| `source_code` | The project's source repository |
| `page_url` | `https://flathub.org/apps/{id}` |

The app's other stores come from the `apps` also-on family.

## Examples

- `https://flathub.org/apps/org.signal.Signal`
