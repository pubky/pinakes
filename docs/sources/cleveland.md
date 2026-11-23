# Cleveland Museum of Art

Artworks of the Cleveland Museum of Art, read from its Open Access API by accession number (or
numeric id). The data is CC0; no key; no published rate limit (one request per second).

## Links

`cleveland:{accession number}` and every clevelandart.org address of an artwork are one
resource, with `cleveland:{accession number}` as the root (the museum's own number, in lower case,
e.g. `cleveland:1958.39` or `cleveland:1916.1511.a`):

- `https://www.clevelandart.org/art/{accession number}`;
- `https://openaccess-api.clevelandart.org/api/artworks/{accession number}`.

The site and the API also take the artwork's numeric id (`https://www.clevelandart.org/art/135382`,
`https://openaccess-api.clevelandart.org/api/artworks/135382`). Such a link is named by the API's
answer: after the fetch it is an alias of `cleveland:{accession number}`.

An accession number or id the API does not know (404 "Artwork not found") is `missing`.

## What it reads

`GET https://openaccess-api.clevelandart.org/api/artworks/{accession number or id}`.

## Card

`VisualArtwork`, with `site` "Cleveland Museum of Art", `name` the title and `description` the
museum's own text as plain text. `image` is the web-sized image, only for works the API marks
`CC0`. The API gives only years, so the card has no `start`; they are facts.

| Fact | Meaning |
| --- | --- |
| `creators` | Each creator as the museum writes them, e.g. `Claude Monet (French, 1840–1926)` |
| `creation_date` | The date as the museum writes it, e.g. `c. 1868–73` |
| `begin_year`, `end_year` | The years the date spans |
| `original_title` | The title in its original language |
| `technique`, `measurements` | As the museum writes them |
| `department`, `collection`, `type` | E.g. `Modern European Painting and Sculpture`, `Painting` |
| `culture` | E.g. `France, 19th century` |
| `current_location` | The gallery, when on view |
| `credit_line`, `copyright` | The credit line, and the copyright notice of a copyrighted work |
| `accession_number`, `former_accession_numbers` | The museum's numbers |
| `object_id` | The numeric id |
| `share_license_status` | `CC0` or `Copyrighted` |
| `wikidata` | The work's Wikidata id (only when the museum names exactly one) |
| `page_url` | The artwork's page on clevelandart.org |

## Groups

The work's Wikidata id is also its key `wikidata:Q…` (the `own_ids` also-on source): the card
joins the group of the Wikidata item's card.

## Licence and attribution

The data is CC0. The card credits "Cleveland Museum of Art", with ", CC0" for CC0 works.

## Examples

- `https://www.clevelandart.org/art/1958.39` (Monet, *The Red Kerchief*)
- `cleveland:1958.39`
