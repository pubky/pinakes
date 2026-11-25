# Cleveland Museum of Art

Pinakes reads artworks of the Cleveland Museum of Art from the museum's Open Access API, by
accession number or numeric ID. No key is needed.

| | |
| --- | --- |
| Source | `cleveland`, official source |
| Links | `clevelandart.org/art/{accession number or id}`, `openaccess-api.clevelandart.org/api/artworks/{accession number or id}`; root `cleveland:{accession number}` |
| Fetches | `GET https://openaccess-api.clevelandart.org/api/artworks/{accession number or id}` |
| Credentials | None |
| Rate | 1 request per second on `openaccess-api.clevelandart.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `VisualArtwork` |
| Keys | `wikidata:Q…`, the Wikidata item the museum's record names for the work |
| Credit | "Cleveland Museum of Art", with ", CC0" for CC0 works, in `attribution` |
| Switch | `[sources.cleveland] enabled`, on by default |

## Links

Every address of one artwork is one resource. The root is `cleveland:{accession number}`, the
museum's own number in lower case, such as `cleveland:1958.39` or `cleveland:1916.1511.a`. These
links are [aliases](../api.md#aliases) of it:

- `https://www.clevelandart.org/art/{accession number}`;
- `https://openaccess-api.clevelandart.org/api/artworks/{accession number}`, with any query.

An accession number has at least two parts separated by dots. Each part is up to 12 digits or
letters, and the first part is digits only; the whole number is at most 32 characters. A link's
accession number is read in any letter case and kept in lower case. A `cleveland:` root must hold
a valid accession number in lower case. Any other `cleveland:` root is `missing` at once and never
fetched.

### Links by numeric ID

The site and the API also take the artwork's numeric ID (up to 10 digits), such as
`https://www.clevelandart.org/art/135382` or
`https://openaccess-api.clevelandart.org/api/artworks/135382`. The link alone doesn't name the
accession number, so the API's answer names the card. Once the card is stored, the link is an
alias of `cleveland:{accession number}`.

### Not read here

Pages with a title after the number (such as `/art/1958.39-the-red-kerchief`), deeper paths,
collection searches and other API objects such as creators.

## What it fetches

One request per read:
`GET https://openaccess-api.clevelandart.org/api/artworks/{accession number or id}`. The API takes
either form. An accession number or ID the API doesn't know (404, "Artwork not found") is
`missing`. An answer for another artwork is an error, and the read is retried.

## Card

The type is `VisualArtwork`.

- `site` is "Cleveland Museum of Art" and `name` the title.
- `description` is the museum's description, or else its wall text, as plain text.
- `image` is the web-sized image, only for works the API marks `CC0` and only as an `https` URL.
- The API gives only years, so the card has no `start`. The dates are facts.
- The `page_url` fact is the artwork's page,
  `https://www.clevelandart.org/art/{accession number}`. Because the root
  `cleveland:{accession number}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "Cleveland Museum of Art, CC0" for a CC0 work, and "Cleveland Museum of Art"
  for any other.

| Fact | Meaning |
| --- | --- |
| `creators` | Each creator as the museum writes them, for example `Claude Monet (French, 1840–1926)` |
| `creation_date` | The date as the museum writes it, for example `c. 1868–73` |
| `begin_year`, `end_year` | The years the date spans |
| `original_title` | The title in its original language |
| `technique`, `measurements` | As the museum writes them |
| `department`, `collection`, `type` | For example `Modern European Painting and Sculpture`, `Painting` |
| `culture` | For example `France, 19th century` |
| `current_location` | The gallery, when the work is on view |
| `credit_line`, `copyright` | The credit line, and the copyright notice of a copyrighted work |
| `accession_number`, `former_accession_numbers` | The museum's current and former numbers |
| `object_id` | The numeric ID |
| `share_license_status` | `CC0` or `Copyrighted` |
| `wikidata` | The work's Wikidata ID, only when the museum names exactly one |
| `page_url` | The artwork's page on clevelandart.org |

## Keys and groups

The work's Wikidata item, when the museum's record names exactly one, is its key `wikidata:Q…`
(the `own_ids` also-on source records it). The card then joins the
[group](../api.md#groups) of the Wikidata item's card.

## Credit

The `attribution` line credits the Cleveland Museum of Art, and adds CC0 for a work the API marks
`CC0`, the only kind whose image the card shows. The API's data is CC0.

## Turn it off

```toml
[sources.cleveland]
enabled = false
```

clevelandart.org links then get ordinary web cards, and `cleveland:` roots wait until the source is
on again.

## Examples

- `https://www.clevelandart.org/art/1958.39` (Monet, *The Red Kerchief*)
- `cleveland:1958.39`
