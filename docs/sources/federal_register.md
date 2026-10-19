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
