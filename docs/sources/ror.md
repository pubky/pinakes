# ROR

Research organizations from the Research Organization Registry, read from its v2 API by ROR id.
CC0; no key; one request per second. Cards carry the attribution "Research Organization Registry
(CC0)".

## Links

A ROR id is `0`, six Crockford base32 characters and two check digits (ISO 7064 mod 97-10 over
the base32 value), e.g. `03yrm5c26`; it is read in any case and kept in lower case. An id with a
wrong check is not a ROR link. `ror:{id}` and every address of an organization are one resource:

- `https://ror.org/{id}` (`http`, with or without `www.`);
- `https://api.ror.org/v2/organizations/{id}`, and the older `…/v1/organizations/{id}` and
  `…/organizations/{id}`.

An id the API does not know (404) is `missing`.

## What it reads

`GET https://api.ror.org/v2/organizations/{id}`.

## Card

`name` is the organization's display name, `site` "ROR". The location is set when ROR gives
exactly one location. The year an organization was established is a fact, not a start date.

The type follows ROR's types: `education` → `EducationalOrganization`, `healthcare` →
`MedicalOrganization`, `government` → `GovernmentOrganization`, `company` → `Corporation`,
`nonprofit` → `NGO`, `archive` → `ArchiveOrganization`, `facility` → `ResearchOrganization`
(`funder` and `other` say nothing about the kind). When the types map to exactly one of these, the
card has it; otherwise it is `Organization`.

| Fact | Meaning |
| --- | --- |
| `established` | The year it was established |
| `types` | ROR's types, e.g. `education; funder` |
| `status` | `active`, `inactive` or `withdrawn` |
| `city`, `country`, `country_code` | Its one location (GeoNames) |
| `website` | Its website |
| `acronyms`, `other_names`, `labels` | Other names: acronyms, other names ROR lists (its aliases), names in other languages |
| `domains` | Its web domains |
| `wikidata`, `isni`, `fundref`, `grid` | Its id in each registry, when ROR names one preferred id or only one (ISNI without spaces) |
| `page_url` | `https://ror.org/{id}` |

## Groups

The organization's Wikidata id is also its key `wikidata:Q…` (the `own_ids` also-on source): the
card joins the group of the Wikidata item's card. Its ISNI, Crossref Funder and GRID ids are
facts only and join nothing.

## Examples

- `https://ror.org/03yrm5c26` (California Digital Library, `ArchiveOrganization`)
- `https://ror.org/042nb2s44` (Massachusetts Institute of Technology, `EducationalOrganization`)
