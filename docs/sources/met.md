# The Met

Pinakes reads works in The Metropolitan Museum of Art from the museum's collection API, by object
ID. No key is needed.

| | |
| --- | --- |
| Source | `met`, official source |
| Links | `metmuseum.org/art/collection/search/{id}` (with or without a language prefix), the older `metmuseum.org/collection/the-collection-online/search/{id}`, `collectionapi.metmuseum.org/public/collection/v1/objects/{id}`; root `met:{id}` |
| Fetches | `GET https://collectionapi.metmuseum.org/public/collection/v1/objects/{id}` |
| Credentials | None |
| Rate | 1 request per second on `collectionapi.metmuseum.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `VisualArtwork` |
| Keys | `wikidata:Q…`, the Wikidata item the museum's record names for the work |
| Credit | "The Metropolitan Museum of Art", with ", CC0" for public-domain works, in `attribution` |
| Switch | `[sources.met] enabled`, on by default |

## Links

Every address of one object is one resource. The root is `met:{objectID}`, the ID without leading
zeros, such as `met:436535`. These links are [aliases](../api.md#aliases) of it:

- `https://www.metmuseum.org/art/collection/search/{id}`, with any query (such as `?img=0`);
- the same with a two-letter language prefix, such as
  `https://www.metmuseum.org/fr/art/collection/search/{id}`;
- the older `https://www.metmuseum.org/collection/the-collection-online/search/{id}`;
- `https://collectionapi.metmuseum.org/public/collection/v1/objects/{id}`.

An object ID is a positive decimal number of up to 10 digits. In a link, leading zeros name the
same object. A `met:` root that is not a valid ID without leading zeros is `missing` at once and
never fetched.

Not read here: deeper paths such as `…/search/{id}/extra`, the Heilbrunn Timeline pages
(`/toah/…`) and API searches.

## What it fetches

One request per read: `GET https://collectionapi.metmuseum.org/public/collection/v1/objects/{id}`.
An ID the API doesn't know (404, "ObjectID not found") is `missing`. An answer for another object
is an error, and the read is retried.

## Card

The type is `VisualArtwork`.

- `site` is "The Met" and `name` the title. The API has no description text, so the card has
  none.
- `image` is shown only for a public-domain work: the web-sized image (`primaryImageSmall`), or the
  full-size original (`primaryImage`) when the answer has no web-sized one, and only as an `https`
  URL.
- The API gives only years, so the card has no `start`. The dates are facts.
- The `page_url` fact is the object's page,
  `https://www.metmuseum.org/art/collection/search/{id}`. Because the root `met:{id}` is not a web
  link, the API also lists that page in `open_in`.
- `attribution` is "The Metropolitan Museum of Art, CC0" for a public-domain work, and "The
  Metropolitan Museum of Art" for any other.

| Fact | Meaning |
| --- | --- |
| `artist`, `artist_bio` | The artist's name, and the Met's line about them (nationality, places, dates) |
| `artist_wikidata` | The artist's Wikidata ID (another object than the work) |
| `date` | The date as the Met writes it, for example `1889` or `ca. 1650` |
| `begin_year`, `end_year` | The years the date spans |
| `object_name`, `classification` | For example `Painting`, `Paintings` |
| `medium`, `dimensions` | As the Met writes them |
| `department`, `culture` | For example `European Paintings` |
| `accession_number`, `credit_line` | For example `1993.132`, `Purchase, The Annenberg Foundation Gift, 1993` |
| `gallery_number` | The gallery the work is shown in, when it is on view |
| `public_domain` | `true` when the work is in the public domain |
| `wikidata` | The work's Wikidata ID |
| `page_url` | The object's page on metmuseum.org |

## Keys and groups

The work's Wikidata item, as the museum's record names it, is its key `wikidata:Q…` (the
`own_ids` also-on source records it). The card then joins the
[group](../api.md#groups) of the Wikidata item's card. `artist_wikidata` names another object and
is a fact only.

## Credit

The `attribution` line credits The Metropolitan Museum of Art, and adds CC0 for a public-domain
work, the only kind whose image the card shows. The API's data is CC0.

## Turn it off

```toml
[sources.met]
enabled = false
```

metmuseum.org links then get ordinary web cards, and `met:` roots wait until the source is on
again.

## Examples

- `https://www.metmuseum.org/art/collection/search/436535` (Van Gogh, *Wheat Field with Cypresses*)
- `met:436535`
