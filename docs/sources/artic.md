# Art Institute of Chicago

Artworks of the Art Institute of Chicago, read from its public API by id. No key; 60 requests per
minute (one per second). Every request names the operator in the `AIC-User-Agent` header the API
asks for (the contact User-Agent, see [Politeness](README.md#politeness)); without a contact set,
the header is left out.

## Links

`artic:{id}` and every artic.edu address of an artwork are one resource, with `artic:{id}` as
the root (the id without leading zeros, e.g. `artic:27992`):

- `https://www.artic.edu/artworks/{id}` and `https://www.artic.edu/artworks/{id}/{slug}` (any slug);
- `https://api.artic.edu/api/v1/artworks/{id}`.

An id the API does not know (404 "Not found") is `missing`.

## What it reads

`GET https://api.artic.edu/api/v1/artworks/{id}?fields=…`, limited to the fields the card reads.

## Card

`VisualArtwork`, with `site` "Art Institute of Chicago", `name` the title and `description` the
short description as plain text (else the long one). The card has no `image`: the museum's IIIF
image server answers 403 to any request without the `AIC-User-Agent` header, so a browser can't
load it; the image's id is a fact. The API gives only years, so the card has no `start`.

| Fact | Meaning |
| --- | --- |
| `artist_display` | The artist as the museum writes them, e.g. `Georges Seurat (French, 1859–1891)` |
| `date_display` | The date as the museum writes it, e.g. `1884–86, border added 1888–89` |
| `begin_year`, `end_year` | The years the date spans |
| `medium`, `dimensions` | As the museum writes them |
| `place_of_origin`, `department`, `artwork_type` | E.g. `France`, `Painting and Sculpture of Europe`, `Painting` |
| `main_reference_number`, `credit_line` | E.g. `1926.224`, `Helen Birch Bartlett Memorial Collection` |
| `copyright_notice` | The copyright notice of a copyrighted work |
| `public_domain` | `true` when the work is in the public domain |
| `image_id` | The id of the work's image on the museum's IIIF server |
| `page_url` | The artwork's page on artic.edu |

## Licence and attribution

The API's `description` field is CC BY 4.0; all its other data is CC0 1.0 (and the Terms and
Conditions of artic.edu apply). The card credits "Art Institute of Chicago", with ", CC0 1.0" for
public-domain works, and "; description CC BY 4.0" when the description is the long one.

## Examples

- `https://www.artic.edu/artworks/27992` (Seurat, *A Sunday on La Grande Jatte — 1884*)
- `artic:27992`
