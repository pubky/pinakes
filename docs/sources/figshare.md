# Figshare

Articles on Figshare and its institutional portals (datasets, figures, papers, presentations,
software and more), read from the Figshare API by article id. No key; Figshare asks for about one
request a second.

## Links

Every address of one article, in any version, is one resource, with `figshare:{id}` as the root:

- `https://figshare.com/articles/{type}/{slug}/{id}` and `…/{id}/{version}`;
- the older `https://figshare.com/articles/{slug}/{id}` and `…/{id}/{version}`, and
  `https://figshare.com/articles/{id}`;
- the same on an institutional portal, `https://{portal}.figshare.com/articles/…` (portals share
  figshare.com's article ids);
- `https://api.figshare.com/v2/articles/{id}` and `…/versions/{version}`.

The card is the article as a whole, read from its latest version. Portals on their own domains
are not recognised from the link; the card's page is the one the API names (on the article's
portal). Private links (`figshare.com/s/…`), files, collections, projects and DOI links
(`doi.org/10.6084/m9.figshare.{id}`, read by the DOI route) are not read here.

An article the API doesn't know is `missing`.

## What it reads

`GET https://api.figshare.com/v2/articles/{id}`.

## Card

The type follows the item type: `Dataset`, `SoftwareSourceCode`, `ImageObject` (a figure),
`MediaObject`, `ScholarlyArticle` (a journal or conference contribution, a preprint), `Thesis`,
`Book`, `Chapter`, `Report`, else `CreativeWork` (posters and presentations among them). `site`
is "Figshare", `name` the title, `description` the description as plain text, and `start` the
latest version's publication time. `image` is Figshare's thumbnail, only under CC0 or CC BY.
`attribution` is the licence, e.g. "CC BY 4.0".

| Fact | Meaning |
| --- | --- |
| `doi` | The latest version's DOI, and the article's own DOI (without the `.v{n}` suffix) when Figshare minted it; a publisher's DOI for some portal articles |
| `authors` | The authors, by name |
| `license` | The licence, e.g. "CC BY 4.0" |
| `item_type` | E.g. "dataset", "presentation", "journal contribution" |
| `version` | The latest version's number |
| `categories` | The research categories |
| `tags` | The article's keywords |
| `page_url` | The article's page on figshare.com or its portal |

## Licence

Figshare's metadata is CC0; each article's content is under the licence it states.

## Examples

- `https://figshare.com/articles/dataset/Garden_data_set/1018700` (a dataset)
- `https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445`
  (a presentation in its second version)
