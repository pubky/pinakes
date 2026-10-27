# GLEIF

Legal Entity Identifier (LEI) records from the Global Legal Entity Identifier Foundation's API.
CC0; no key; GLEIF allows 60 requests per minute (one per second here).

## Links

`lei:{LEI}` is the root; `https://www.gleif.org/lei/{LEI}` and
`https://api.gleif.org/api/v1/lei-records/{LEI}` are aliases of it. The record's sub-resources
(`/direct-parent`, `/ultimate-parent`, `/direct-children`, …) name other entities and are not
read. An LEI is 20 characters with two check digits (ISO 17442); one that fails the check is
never read.

GLEIF's search page names a record only in the URL fragment
(`https://search.gleif.org/#/record/{LEI}`). A resource's identity drops fragments, so these
links all name the same page and are not read as records.

## What it reads

`GET https://api.gleif.org/api/v1/lei-records/{LEI}` (about 3 KB). An LEI GLEIF does not know is
`missing`.

## Card

An `Organization` named by its legal name, described by its LEI, legal seat and entity status, with
the legal address in `address`.

| Fact | Meaning |
| --- | --- |
| `lei` | The LEI |
| `jurisdiction` | E.g. `CH`, `US-DE` |
| `legal_form` | The ISO 20275 entity legal form code (e.g. `MVII`) or GLEIF's free text |
| `entity_status`, `registration_status` | E.g. `ACTIVE`; `ISSUED`, `LAPSED`, `RETIRED` |
| `registered_as`, `registered_at` | The entity's number at its business register, and that register's GLEIF code |
| `bic` | Its BIC codes |
| `opencorporates` | Its OpenCorporates id |
| `last_update` | When the record last changed (`YYYY-MM-DD`) |
| `page_url` | The record on GLEIF's search site |

## Examples

- `lei:KY37LUS27QQX7BB93L28`
- `https://api.gleif.org/api/v1/lei-records/5493001KJTIIGC8Y1R12`
