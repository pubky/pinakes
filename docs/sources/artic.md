# Art Institute of Chicago

Pinakes reads artworks of the Art Institute of Chicago from the museum's public API, by artwork ID.
No key is needed. Each request names the operator in the `AIC-User-Agent` header the API asks
for.

| | |
| --- | --- |
| Source | `artic`, official source |
| Links | `artic.edu/artworks/{id}` and `artic.edu/artworks/{id}/{slug}`, `api.artic.edu/api/v1/artworks/{id}`; root `artic:{id}` |
| Fetches | `GET https://api.artic.edu/api/v1/artworks/{id}?fields=…`, with an `AIC-User-Agent` header |
| Credentials | None. The header carries the operator's contact (`http.contact`) and is left out while that is empty |
| Rate | 1 request per second on `api.artic.edu` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `VisualArtwork` |
| Keys | None (facts only) |
| Credit | "Art Institute of Chicago", with the CC0 licence of a public-domain work and the CC BY licence of a long description, in `attribution` |
| Switch | `[sources.artic] enabled`, on by default |

## Links

Every address of one artwork is one resource. The root is `artic:{id}`, the ID without leading
zeros, such as `artic:27992`. These links are [aliases](../api.md#aliases) of it:

- `https://www.artic.edu/artworks/{id}` and `https://www.artic.edu/artworks/{id}/{slug}`, with any
  slug;
- `https://api.artic.edu/api/v1/artworks/{id}`, with any query.

An artwork ID is a positive decimal number of up to 10 digits. In a link, leading zeros name the
same artwork. An `artic:` root that is not a valid ID without leading zeros is `missing` at once
and never fetched.

Not read here: deeper paths such as `/artworks/{id}/{slug}/extra`, artist pages, the API's IIIF
manifests (`/manifest.json`) and API searches.

## What it fetches

One request per read, limited to the fields the card reads:

`GET https://api.artic.edu/api/v1/artworks/{id}?fields=…`

The request carries the `AIC-User-Agent` header with the same contact User-Agent as every request
(`pinakes/<version> (+https://pubky.app; <contact>)`, see [Politeness](README.md#politeness)).
While `http.contact` is empty, the header is left out.

An ID the API doesn't know (404, "Not found") is `missing`. An answer for another artwork is an
error, and the read is retried.

## Card

The type is `VisualArtwork`.

- `site` is "Art Institute of Chicago" and `name` the title.
- `description` is the short description as plain text, or else the long one.
- The card has no `image`. The museum's IIIF image server answers 403 to any request without the
  `AIC-User-Agent` header, so a browser can't load the image. The image's ID is the `image_id`
  fact.
- The API gives only years, so the card has no `start`. The dates are facts.
- The `page_url` fact is the artwork's page, `https://www.artic.edu/artworks/{id}`. Because the
  root `artic:{id}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "Art Institute of Chicago". It adds ", CC0 1.0" for a public-domain work, and
  "; description CC BY 4.0" when the description is the long one.

| Fact | Meaning |
| --- | --- |
| `artist_display` | The artist as the museum writes them, for example `Georges Seurat (French, 1859–1891)` |
| `date_display` | The date as the museum writes it, for example `1884–86, border added 1888–89` |
| `begin_year`, `end_year` | The years the date spans |
| `medium`, `dimensions` | As the museum writes them |
| `place_of_origin`, `department`, `artwork_type` | For example `France`, `Painting and Sculpture of Europe`, `Painting` |
| `main_reference_number`, `credit_line` | For example `1926.224`, `Helen Birch Bartlett Memorial Collection` |
| `copyright_notice` | The copyright notice of a copyrighted work |
| `public_domain` | `true` when the work is in the public domain |
| `image_id` | The ID of the work's image on the museum's IIIF server |
| `page_url` | The artwork's page on artic.edu |

## Keys and groups

None. The card joins no [group](../api.md#groups).

## Credit

The `attribution` line credits the Art Institute of Chicago. The API's long `description` is
CC BY 4.0, and all its other data is CC0 1.0. That is why the card prefers the short description,
and credits CC BY 4.0 when it shows the long one.

## Turn it off

```toml
[sources.artic]
enabled = false
```

artic.edu links then get ordinary web cards, and `artic:` roots wait until the source is on again.

## Examples

- `https://www.artic.edu/artworks/27992` (Seurat, *A Sunday on La Grande Jatte — 1884*)
- `artic:27992`
