# ROR

Pinakes reads research organizations in the Research Organization Registry from its v2 API, by ROR
ID. No key is needed.

| | |
| --- | --- |
| Source | `ror`, official source |
| Links | `ror.org/{id}`, `api.ror.org/v2/organizations/{id}` and the older API paths; root `ror:{id}` |
| Fetches | `GET https://api.ror.org/v2/organizations/{id}` |
| Credentials | None |
| Rate | 1 request per second on `api.ror.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `EducationalOrganization`, `MedicalOrganization`, `GovernmentOrganization`, `Corporation`, `NGO`, `ArchiveOrganization`, `ResearchOrganization`, `Organization` |
| Keys | `wikidata:Q…`, the Wikidata item ROR names for the organization |
| Credit | "Research Organization Registry (CC0)", in `attribution` |
| Switch | `[sources.ror] enabled`, on by default |

## Links

A ROR ID is `0`, six Crockford base32 characters and two check digits (ISO 7064 mod 97-10 over the
base32 value), such as `03yrm5c26`. A link's ID is read in any letter case and kept in lower case.
An ID with a wrong check is not a ROR link.

Every address of one organization is one resource. The root is `ror:{id}`, and these links are
[aliases](../api.md#aliases) of it:

- `https://ror.org/{id}`, also over `http` and with `www.`;
- `https://api.ror.org/v2/organizations/{id}`, and the older `https://api.ror.org/v1/organizations/{id}`
  and `https://api.ror.org/organizations/{id}`.

A `ror:` root must hold a valid ID in lower case. Any other `ror:` root is `missing` at once and
never fetched.

Not read here: searches on ror.org or the API, and deeper paths such as `ror.org/{id}/x`.

## What it fetches

One request per read: `GET https://api.ror.org/v2/organizations/{id}`. An ID the API doesn't know
(404) is `missing`. An answer for another organization is an error, and the read is retried.

## Card

The type follows ROR's types:

| ROR type | Card type |
| --- | --- |
| `education` | `EducationalOrganization` |
| `healthcare` | `MedicalOrganization` |
| `government` | `GovernmentOrganization` |
| `company` | `Corporation` |
| `nonprofit` | `NGO` |
| `archive` | `ArchiveOrganization` |
| `facility` | `ResearchOrganization` |

`funder` and `other` say nothing about the kind. When the organization's types map to exactly one
of the card types above, the card has it; otherwise it is `Organization`. For example,
`education; funder` is `EducationalOrganization`, and `education; healthcare` is `Organization`.

- `site` is "ROR" and `name` the organization's display name. The card has no description and no
  image.
- The location, and the `city`, `country` and `country_code` facts, are set only when ROR gives
  exactly one location.
- The year the organization was established is a fact (`established`), not a `start`.
- The `page_url` fact is `https://ror.org/{id}`. Because the root `ror:{id}` is not a web link, the
  API also lists that page in `open_in`.
- `attribution` is "Research Organization Registry (CC0)".

| Fact | Meaning |
| --- | --- |
| `established` | The year it was established |
| `types` | ROR's types, for example `education; funder` |
| `status` | `active`, `inactive` or `withdrawn` |
| `city`, `country`, `country_code` | Its one location (from GeoNames) |
| `website` | Its website |
| `acronyms` | Its acronyms |
| `other_names` | The other names ROR lists for it (ROR calls them aliases) |
| `labels` | Its names in other languages, without the display name |
| `domains` | Its web domains |
| `wikidata`, `isni`, `fundref`, `grid` | Its ID in each registry, when ROR names a preferred ID or only one ID of that registry (ISNI without spaces) |
| `page_url` | `https://ror.org/{id}` |

ROR's "aliases" are other names, published as `other_names`. They are not the card's
[aliases](../api.md#aliases), which are other links to it.

## Keys and groups

The organization's Wikidata item is its key `wikidata:Q…` (the `own_ids` also-on
source records it). The card then joins the [group](../api.md#groups) of the Wikidata item's card.
Its ISNI, Crossref Funder and GRID IDs are facts only and join nothing.

## Credit

The `attribution` line credits the Research Organization Registry and states the data's licence,
CC0.

## Turn it off

```toml
[sources.ror]
enabled = false
```

ROR links then get ordinary web cards, and `ror:` roots wait until the source is on again.

## Examples

- `https://ror.org/03yrm5c26` (California Digital Library, `ArchiveOrganization`)
- `https://ror.org/042nb2s44` (Massachusetts Institute of Technology, `EducationalOrganization`)
