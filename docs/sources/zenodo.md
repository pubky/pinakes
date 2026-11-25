# Zenodo

Pinakes reads records on Zenodo (datasets, software, papers, images and more) from the Zenodo REST
API, by record ID. No key is needed.

| | |
| --- | --- |
| Source | `zenodo`, official source |
| Links | `zenodo.org/records/{id}`, `zenodo.org/record/{id}`, `zenodo.org/api/records/{id}` |
| Fetches | `GET https://zenodo.org/api/records/{id}` |
| Credentials | None |
| Rate | At most 2.2 requests per second on `zenodo.org` (Zenodo's guest limit of 133 a minute); the shared default of 1 per second applies while it is lower |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Dataset`, `SoftwareSourceCode`, `ScholarlyArticle`, `Book`, `Chapter`, `Report`, `Thesis`, `ImageObject`, `VideoObject`, `CreativeWork` |
| Keys | `doi:…`, only for DOIs Zenodo issued (`10.5281/zenodo.…`) |
| Credit | Creators and licence, in `attribution` |
| Switch | `[sources.zenodo] enabled`, on by default |

## Links

Every address of one record is one resource. The root is `zenodo:{id}`, and these links are
[aliases](../api.md#aliases) of it:

- `https://zenodo.org/records/{id}`;
- `https://zenodo.org/record/{id}` (the older form);
- `https://zenodo.org/api/records/{id}`.

A record ID is decimal digits without a leading zero. A `zenodo:` root that is not a valid ID is
`missing` at once and never fetched.

A record is one version of a work. The work as a whole, all its versions, is a concept record with
an ID of its own, and the API answers that ID with the latest version. A concept ID's card is the
concept: the latest version's title, text, licence and image under the concept's root and page,
with the concept DOI.

Not read here: links to a record's files, `/latest` links, community pages, `sandbox.zenodo.org`
(a test service) and DOI links such as `https://doi.org/10.5281/zenodo.{id}`, which the DOI route
reads.

## What it fetches

One request per read: `GET https://zenodo.org/api/records/{id}`. A record the API doesn't know
(404) or that was deleted (410) is `missing`. An answer for a record that is neither the one asked
for nor its concept is an error, and the read is retried.

## Card

The type follows the record's resource type: `Dataset`, `SoftwareSourceCode`, `ScholarlyArticle` (a
publication; `Book`, `Chapter`, `Report` or `Thesis` for those subtypes), `ImageObject`,
`VideoObject`, else `CreativeWork`.

- `site` is "Zenodo", `name` the title, `description` the description as plain text.
- The record's page, `https://zenodo.org/records/{id}`, is the `page_url` fact. Because the root
  `zenodo:{id}` is not a web link, the API also lists that page in `open_in`.
- `image` is Zenodo's 750-pixel preview of the record's files, only for open records under CC0 or
  CC BY.
- `attribution` names up to three creators, then "et al.", and the licence, for example
  "Coniglio, Michael C.; Corfidi, Stephen F.; Kain, John S. / CC0 1.0".

| Fact | Meaning |
| --- | --- |
| `doi` | The record's DOI as Zenodo states it (a publisher's DOI for some records); the concept DOI on a concept card |
| `creators` | The creators, by name |
| `license` | The licence ID, for example `cc-by-4.0` |
| `resource_type` | For example "Journal article", "Software", "Figure" |
| `access_right` | `open`, `embargoed`, `restricted` or `closed` |
| `communities` | The Zenodo communities the record is in |
| `keywords` | The record's keywords |
| `publication_date` | The publication date as given (`YYYY-MM-DD`, or less precise) |
| `version` | The version the record names |
| `concept_record`, `concept_doi` | The work (all versions) the record is a version of |
| `latest_version` | On a concept card: the latest version's record ID |
| `page_url` | The record's page on zenodo.org |

## Keys and groups

The record's DOI is its key `doi:…` only when Zenodo issued it (`10.5281/zenodo.…`; the
`own_ids` also-on source records it). The record then joins the
[group](../api.md#groups) of the `doi:` card for that DOI, and a concept card joins the group of the
concept DOI's card. A publisher's DOI in `doi`, and a version's `concept_doi`, are facts only and
join nothing.

## Credit

The `attribution` line credits the creators and the licence.

## Turn it off

```toml
[sources.zenodo]
enabled = false
```

Zenodo links then get ordinary web cards, and `zenodo:` roots wait until the source is on again.

## Examples

- `https://zenodo.org/records/1234567` (a journal article with the publisher's DOI)
