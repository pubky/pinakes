# Figshare

Articles on Figshare and its institutional portals (datasets, figures, papers, presentations,
software and more), read from the Figshare API by article id. No key; Figshare asks for about one
request a second.

## Links

Every address of one article is one resource, with `figshare:{id}` as the root:

- `https://figshare.com/articles/{type}/{slug}/{id}`;
- the older `https://figshare.com/articles/{slug}/{id}`, and `https://figshare.com/articles/{id}`;
- the same on an institutional portal, `https://{portal}.figshare.com/articles/…` (portals share
  figshare.com's article ids);
- `https://api.figshare.com/v2/articles/{id}`.

The card is the article as a whole, read from its latest version. A link to one version
(`…/{id}/{version}`, `…/v2/articles/{id}/versions/{version}`) is a dated version, not the
article, and is not read here. Portals on their own domains
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
is "Figshare", `name` the title and `description` the description as plain text; the latest
version's publication time is the `published` fact. `image` is Figshare's thumbnail, only under
CC0 or CC BY.
`attribution` credits the authors (up to three, then "et al.") and the licence, e.g.
"Pehr Söderman / CC BY 4.0".

| Fact | Meaning |
| --- | --- |
| `published` | When the latest version was published (RFC 3339) |
| `doi` | The article's own DOI (without the `.v{n}` suffix) when Figshare minted it; a publisher's DOI for some portal articles |
| `version_doi` | The version DOI (`….{id}.v{n}`) the record names, when Figshare minted it |
| `authors` | The authors, by name |
| `license` | The licence, e.g. "CC BY 4.0" |
| `item_type` | E.g. "dataset", "presentation", "journal contribution" |
| `version` | The latest version's number |
| `categories` | The research categories |
| `tags` | The article's keywords |
| `page_url` | The article's page on figshare.com or its portal |

## Groups

The article's DOI, when Figshare issued it (`10.6084/m9.figshare.{id}`), is also its key `doi:…`
(the `own_ids` also-on source): the article joins the group of the `doi:` card of that DOI. A
`version_doi`, and a publisher's DOI in `doi`, are facts only and join nothing.

## Licence

Figshare's metadata is CC0; each article's content is under the licence it states.

## Examples

- `https://figshare.com/articles/dataset/Garden_data_set/1018700` (a dataset)
- `https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445`
  (a presentation in its second version)
