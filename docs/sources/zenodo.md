# Zenodo

Records on Zenodo (datasets, software, papers, images and more), read from the Zenodo REST API
by record id. No key; guests may make 133 requests a minute (2.2 per second).

## Links

Every address of one record is one resource, with `zenodo:{id}` as the root:

- `https://zenodo.org/records/{id}`;
- `https://zenodo.org/record/{id}` (the older form);
- `https://zenodo.org/api/records/{id}`.

A record is one version of a work. The work as a whole (all its versions) is a concept record
with an id of its own; the API redirects that id to the latest version. A concept id's card is
the concept: the latest version's title and text under the concept's root and page, with the
concept DOI. Links to a record's files, `sandbox.zenodo.org` (a test service) and DOI links
(`doi.org/10.5281/zenodo.{id}`, read by the DOI route) are not read here.

A record the API doesn't know (404) or that was deleted (410) is `missing`.

## What it reads

`GET https://zenodo.org/api/records/{id}`.

## Card

The type follows the record's resource type: `Dataset`, `SoftwareSourceCode`, `ScholarlyArticle`
(a publication; `Book`, `Chapter`, `Report` or `Thesis` for those subtypes), `ImageObject`,
`VideoObject`, else `CreativeWork`. `site` is "Zenodo", `name` the title, `description` the
description as plain text; the publication date, as given, is the `publication_date` fact. `image` is
Zenodo's preview of the record's files, only for open records under CC0 or CC BY. `attribution`
is the licence, e.g. "CC BY 4.0".

| Fact | Meaning |
| --- | --- |
| `doi` | The record's DOI, as Zenodo states it (a publisher's DOI for some records); a concept's DOI on a concept card |
| `creators` | The creators, by name |
| `license` | The licence id, e.g. `cc-by-4.0` |
| `resource_type` | E.g. "Journal article", "Software", "Figure" |
| `access_right` | `open`, `embargoed`, `restricted` or `closed` |
| `communities` | The Zenodo communities the record is in |
| `keywords` | The record's keywords |
| `publication_date` | The publication date, as given (`YYYY-MM-DD`, or less precise) |
| `version` | The version the record names |
| `concept_record`, `concept_doi` | The work (all versions) the record is a version of |
| `latest_version` | On a concept card: the latest version's record id |
| `page_url` | The record's page on zenodo.org |

## Licence

Zenodo's metadata is CC0; each record's files and text are under the licence the record states.

## Examples

- `https://zenodo.org/records/1234567` (a journal article with the publisher's DOI)
