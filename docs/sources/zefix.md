# Zefix

Swiss companies in the commercial register, read as linked data from LINDAS, the Confederation's
linked-data service, by the company's register id (EHRA id). The zefix.ch pages are an
application shell without metadata and are never read. No key; one request per second. The card
names its source: "Source: Zefix, Federal Office of Justice (EHRA), via LINDAS".

## Links

`https://register.ld.admin.ch/zefix/company/{id}` is the root; its sub-resources (`/address`,
`/UID/…`, `/CHID/…`) and the company's zefix.ch page,
`https://www.zefix.ch/{lang}/search/entity/list/firm/{id}`, are aliases of it. Searches on
zefix.ch are not read. One UID (`CHE-…`) can belong to several register entries (a company
registered at two seats), so a UID alone does not name one company and is not read as one.

## What it reads

`GET https://ld.admin.ch/query?query=…`: every statement about the company, its address and its
identifiers, and the names of its legal form and seat (about 14 KB). An id with no company is
`missing`.

## Card

An `Organization` named by its legal name, described by its purpose, with its registered address
in `details.address`.

| Fact | Meaning |
| --- | --- |
| `uid` | Its business identification number, e.g. `CHE-105.909.036` |
| `ch_id`, `ehra_id` | Its register ids |
| `legal_form` | E.g. Aktiengesellschaft |
| `seat`, `canton` | Its registered seat |
| `other_names` | Its name in other languages, e.g. `Nestlé S.A. (fr)` |
| `page_url` | Its page on zefix.ch |

## Examples

- `https://register.ld.admin.ch/zefix/company/126286`
