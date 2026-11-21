# Federal Register

Documents published in the US Federal Register (rules, proposed rules, notices and presidential
documents), read from the official API by document number. The HTML pages are behind a bot wall
and are never read. Public domain; no key; no published rate limit (one request per second).

## Links

`fr:{number}` and every federalregister.gov address of a document are one resource, with
`fr:{number}` as the root (the number in upper case, e.g. `fr:E9-12345`):

- `https://www.federalregister.gov/documents/{yyyy}/{mm}/{dd}/{number}/{slug}` (any slug, or none);
- `https://www.federalregister.gov/d/{number}`;
- `https://www.federalregister.gov/documents/{number}`;
- `https://www.federalregister.gov/api/v1/documents/{number}` and `….json`.

A number the API does not know is `missing`.

## What it reads

`GET https://www.federalregister.gov/api/v1/documents/{number}.json`.

## Card

`Legislation`, with `site` "Federal Register", `name` the title and `description` the abstract
(else the action line).

| Fact | Meaning |
| --- | --- |
| `document_type` | Rule, Proposed Rule, Notice or Presidential Document |
| `agencies` | The agencies, by name |
| `citation` | E.g. `91 FR 57392` |
| `publication_date` | `YYYY-MM-DD` |
| `docket_ids` | The docket identifiers the document lists |
| `regulation_id_numbers` | Regulation identifier numbers (RINs) |
| `comments_close_on`, `effective_on` | `YYYY-MM-DD`, when the document has them |
| `cfr_references` | E.g. `14 CFR 39` |
| `page_count` | Pages in the printed edition |
| `page_url` | The document's page on federalregister.gov |

## Examples

- `fr:2026-18291` (a notice)
- `https://www.federalregister.gov/documents/2026/09/09/2026-18291/self-regulatory-organizations-nasdaq-texas-llc-notice-of-filing-and-order-granting-accelerated`

## Also on

`federal_register`: a document gets its GovInfo PDF (`kind: "pdf"`) and details page, its
regulations.gov document and its public-inspection PDF (`kind: "pdf"` too) from its API record, and the key
`fr:{number}`. Two other copies someone tags get the same key, so they join the document:

- a GovInfo copy (`https://www.govinfo.gov/content/pkg/FR-{date}/pdf/{number}.pdf` or
  `https://www.govinfo.gov/app/details/FR-{date}/{number}`), only when the day of its package is
  the document's `publication_date` in the API record (read once an hour); an address with
  another day gets no key;
- the SEC's PDF of a self-regulatory organization's rule-change release
  (`https://www.sec.gov/files/rules/sro/{org}/{year}/{release}.pdf`; an exhibit,
  `{release}-{exhibit}.pdf`, is part of the filing and gets no key), when exactly one document's
  docket ids name `Release No. {release}`: `GET https://www.federalregister.gov/api/v1/documents.json?conditions[docket_id]={release}&fields[]=document_number&fields[]=docket_ids&per_page=20`.

### Keys

`fr:{document number}`: each names one object, so a [group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds the links that carry it.
