# The Met

Works in The Metropolitan Museum of Art, read from its collection API by object id. The API's
data is CC0; no key; up to 80 requests per second are allowed (Pinakes sends one per second).

## Links

`met:{objectID}` and every metmuseum.org address of an object are one resource, with
`met:{objectID}` as the root (the id without leading zeros, e.g. `met:436535`):

- `https://www.metmuseum.org/art/collection/search/{id}` (any query, such as `?img=0`);
- the same with a language prefix, e.g. `https://www.metmuseum.org/fr/art/collection/search/{id}`;
- the older `https://www.metmuseum.org/collection/the-collection-online/search/{id}`;
- `https://collectionapi.metmuseum.org/public/collection/v1/objects/{id}`.

An id the API does not know (404 "ObjectID not found") is `missing`.

## What it reads

`GET https://collectionapi.metmuseum.org/public/collection/v1/objects/{id}`.

## Card

`VisualArtwork`, with `site` "The Met" and `name` the title. The API has no description text, so
the card has none. `image` is the web-sized image (`primaryImageSmall`), or the full-size
original (`primaryImage`) when the answer has no web-sized one, only for public-domain works.
The API gives only years, so the card has no `start`; they are facts.

| Fact | Meaning |
| --- | --- |
| `artist`, `artist_bio` | The artist's name and the Met's line about them (nationality, places, dates) |
| `artist_wikidata` | The artist's Wikidata id (a different object from the work) |
| `date` | The date as the Met writes it, e.g. `1889` or `ca. 1650` |
| `begin_year`, `end_year` | The years the date spans |
| `object_name`, `classification` | E.g. `Painting`, `Paintings` |
| `medium`, `dimensions` | As the Met writes them |
| `department`, `culture` | E.g. `European Paintings` |
| `accession_number`, `credit_line` | E.g. `1993.132`, `Purchase, The Annenberg Foundation Gift, 1993` |
| `gallery_number` | The gallery the work is shown in, when on view |
| `public_domain` | `true` when the work is in the public domain |
| `wikidata` | The work's Wikidata id |
| `page_url` | The object's page on metmuseum.org |

## Groups

The work's Wikidata id is also its key `wikidata:Q…` (the `own_ids` also-on source): the card
joins the group of the Wikidata item's card. `artist_wikidata` names another object and is a fact
only.

## Licence and attribution

The data is CC0. The card credits "The Metropolitan Museum of Art", with ", CC0" for
public-domain works (whose images are CC0 too).

## Examples

- `https://www.metmuseum.org/art/collection/search/436535` (Van Gogh, *Wheat Field with Cypresses*)
- `met:436535`
