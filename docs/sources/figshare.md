# Figshare

Pinakes reads articles on Figshare and its institutional portals (datasets, figures, papers,
presentations, software and more) from the Figshare API, by article ID. No key is needed.

| | |
| --- | --- |
| Source | `figshare`, official source |
| Links | `figshare.com/articles/…/{id}`, `{portal}.figshare.com/articles/…/{id}`, `api.figshare.com/v2/articles/{id}`; root `figshare:{id}` |
| Fetches | `GET https://api.figshare.com/v2/articles/{id}` |
| Credentials | None |
| Rate | 1 request per second on `api.figshare.com` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Dataset`, `SoftwareSourceCode`, `ImageObject`, `MediaObject`, `ScholarlyArticle`, `Thesis`, `Book`, `Chapter`, `Report`, `CreativeWork` |
| Keys | `doi:…`, only for DOIs Figshare issued (`10.6084/m9.figshare.…`) |
| Credit | Authors and licence, in `attribution` |
| Switch | `[sources.figshare] enabled`, on by default |

## Links

Every address of one article is one resource. The root is `figshare:{id}`, and these links are
[aliases](../api.md#aliases) of it:

- `https://figshare.com/articles/{type}/{slug}/{id}`, where `{type}` is one of Figshare's item
  type segments, such as `dataset`, `figure` or `journal_contribution`;
- `https://figshare.com/articles/{slug}/{id}` (the older form) and
  `https://figshare.com/articles/{id}`;
- the same paths on an institutional portal, `https://{portal}.figshare.com/articles/…`. Portals
  share figshare.com's article IDs;
- `https://api.figshare.com/v2/articles/{id}`.

An article ID is up to 10 decimal digits without a leading zero. A `figshare:` root that is not a
valid ID is `missing` at once and never fetched.

The card is the article as a whole, read from its latest version. A link to one version
(`…/{id}/{version}`, `api.figshare.com/v2/articles/{id}/versions/{version}`) names a dated
version, not the article, and is not read here.

Also not read here: portals on their own domains (they are not recognized from the link), private
links (`figshare.com/s/…`), files (`ndownloader.figshare.com`), collections, projects and DOI links
such as `https://doi.org/10.6084/m9.figshare.{id}`, which the DOI route reads.

## What it fetches

One request per read: `GET https://api.figshare.com/v2/articles/{id}`. The answer is the latest
version of the article. An article the API doesn't know (404) is `missing`. An answer for another
article is an error, and the read is retried.

## Card

The type follows the article's item type:

| Item type | Card type |
| --- | --- |
| dataset | `Dataset` |
| software | `SoftwareSourceCode` |
| figure | `ImageObject` |
| media | `MediaObject` |
| journal contribution, conference contribution, preprint | `ScholarlyArticle` |
| thesis | `Thesis` |
| book, monograph | `Book` |
| chapter | `Chapter` |
| report | `Report` |
| any other (posters and presentations among them) | `CreativeWork` |

- `site` is "Figshare", `name` the title, `description` the description as plain text.
- The `page_url` fact is the article's page that the API names (`figshare_url` or
  `url_public_html`), on figshare.com or a `*.figshare.com` portal. When neither names this
  article on those hosts (an article of a portal on its own domain), it is
  `https://figshare.com/articles/_/{id}`. Because the root `figshare:{id}` is not a web link, the
  API also lists that page in `open_in`.
- `image` is Figshare's thumbnail, only under CC0 or CC BY and only as an `https` URL.
- `attribution` names up to three authors, then "et al.", and the licence, for example
  "Pehr Söderman / CC BY 4.0".
- The publication time is a fact (`published`), not a `start`.

| Fact | Meaning |
| --- | --- |
| `doi` | The article's DOI in lower case: Figshare's article DOI without the `.v{n}` version suffix, or a publisher's DOI for some portal articles |
| `version_doi` | The version DOI the record names (`….{id}.v{n}`), when its `doi` is a version DOI of this article |
| `published` | When the latest version was published (RFC 3339) |
| `authors` | The authors, by name |
| `license` | The licence, for example "CC BY 4.0" |
| `item_type` | For example "dataset", "presentation", "journal contribution" |
| `version` | The latest version's number |
| `categories` | The research categories |
| `tags` | The article's keywords |
| `page_url` | The article's page on figshare.com or its portal |

## Keys and groups

The article's DOI is its key `doi:…` only when Figshare issued it (`10.6084/m9.figshare.…`; the
`own_ids` also-on source records it). The article then joins the
[group](../api.md#groups) of the `doi:` card for that DOI. A `version_doi`, and a publisher's DOI in
`doi`, are facts only and join nothing.

## Credit

The `attribution` line credits the authors and the licence. Each article's content is under the
licence it states, which `license` holds.

## Turn it off

```toml
[sources.figshare]
enabled = false
```

Figshare links then get ordinary web cards, and `figshare:` roots wait until the source is on
again.

## Examples

- `https://figshare.com/articles/dataset/Garden_data_set/1018700` (a dataset)
- `https://figshare.com/articles/presentation/Using_APIs_to_customise_repositories_and_engage_audiences/5616445`
  (a presentation in its second version)
