# Snap Store

Linux apps on the Snap Store, read from the snapcraft.io v2 info API by snap name. No key, no
account; the API asks for a `Snap-Device-Series: 16` header, which every request carries. The
metadata is each publisher's own.

## Links

`https://snapcraft.io/{name}` and `https://snapcraft.io/install/{name}/{distribution}` are one
resource. A snap name is lower-case letters, digits and `-`, at most 40 characters. The site's
own pages at the root (`/store`, `/docs`, `/blog`, `/publisher/…`, …) are not read.

## What it reads

`GET https://api.snapcraft.io/v2/snaps/info/{name}?fields=title,summary,publisher,media,website,license,categories`.
A snap the store does not know (404, `resource-not-found`) is `missing`. Because the request
carries a header, a redirect is refused rather than followed.

## Card

`SoftwareApplication`, with `site` "Snap Store", `name` the snap's title, `description` its summary
and `image` its icon.

| Fact | Meaning |
| --- | --- |
| `developer` | The publisher's display name (who packages the snap, not always the app's developer) |
| `category` | The snap's featured category, else its first |
| `website` | The website the publisher gives |
| `license` | The snap's licence (SPDX) |
| `page_url` | `https://snapcraft.io/{name}` |

The app's other stores come from the `apps` also-on family.

## Examples

- `https://snapcraft.io/signal-desktop`
