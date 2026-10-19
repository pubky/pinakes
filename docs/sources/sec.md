# SEC EDGAR

Filings and companies on the US Securities and Exchange Commission's EDGAR system. A filing's
card comes from its header (`{accession}.hdr.sgml`, about 1 KB); the filed documents themselves,
often several megabytes, are never fetched. Public data; no key. EDGAR's [access
rules](https://www.sec.gov/os/accessing-edgar-data) ask for at most ten requests per second and a
User-Agent with an email address: while `http.contact` is empty, EDGAR links wait (they are retried
hourly) instead of being sent. The source keeps well under that limit: five requests per second
for `www.sec.gov` and two each for `sec.gov` and `data.sec.gov`, never above
`limits.host_requests_per_second` (one by default).

## Links

A **filing** is named by the CIK in its address and its accession number. These are one
resource, with the filing's folder, `https://www.sec.gov/Archives/edgar/data/{cik}/{accession,
18 digits}/`, as the root:

- the folder, with or without its trailing slash;
- `{accession}-index.htm`, `{accession}-index.html` and `{accession}-index-headers.html` in it;
- the full text `{accession}.txt` and the header `{accession}.hdr.sgml`;
- `…/data/{cik}/{accession}.txt` and `…/data/{cik}/{accession}-index.htm` beside it.

A filing listed under several CIKs (co-registrants, or the issuer and reporting owner of an
ownership filing) becomes an alias of the primary filer's folder after its first fetch, and a
document of it an alias of the same document in that folder. The primary filer is the issuer of
an ownership filing (Forms 3, 4 and 5) or the issuing entity of an asset-backed one, else the
first filer or subject company.

A **document** of a filing (the main form or an exhibit,
`…/data/{cik}/{accession}/{file}`) is its own resource; the inline viewer address
`https://www.sec.gov/ix?doc=/Archives/edgar/data/…` is an alias of it.

A **company** is named by its CIK: `https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={cik}`
(the root, CIK in ten digits), `https://www.sec.gov/edgar/browse/?CIK={cik}` and
`https://data.sec.gov/submissions/CIK{cik}.json`. A ticker in place of the CIK is not read.

An **SRO release** (a self-regulatory organization's rule change as the SEC publishes it,
`https://www.sec.gov/files/rules/sro/{org}/{year}/{release}.pdf`, and its exhibits
`{release}-{exhibit}.pdf`) is its own resource. The PDF is never fetched: the card comes from the
Federal Register document that publishes the release, found by its docket id `Release No.
{release}` (an exact match; a release number names one document).

## What it reads

- A filing or document: `…/{accession}.hdr.sgml`; for a document also the filing's
  `{accession}-index.htm`, for the document's type and description.
- A company: `https://data.sec.gov/submissions/CIK{cik}.json`.
- An SRO release: the Federal Register API's documents under docket `{release}`, then that
  document's record (see [federal_register](federal_register.md)).

## Card

A filing is a `Report` named "Form {form} · {primary filer}", e.g. "Form 8-K · Twenty One Capital,
Inc.", described as "Filed {date} · period {date} · items {items}" and, for an ownership filing,
"reporting owner {owners}". A company is a `Corporation` with its
name, described by its industry (SIC description).

| Fact | Meaning |
| --- | --- |
| `form` | The form type, e.g. `S-1`, `8-K`, `10-Q` |
| `accession` | The accession number, e.g. `0001104659-26-000959` |
| `filing_date`, `period` | `YYYY-MM-DD` |
| `items` | The 8-K items, e.g. `5.02; 8.01; 9.01` |
| `company` | The filers, primary first |
| `cik` | The primary filer's (or the company's) CIK |
| `file_number` | The SEC file number, e.g. `333-292586` |
| `reporting_owner` | The reporting owners of an ownership filing |
| `depositor`, `depositor_cik` | An asset-backed filing's depositor (its name when the header has one) |
| `asset_class` | An asset-backed filing's asset class, e.g. `Auto loans` |
| `page_url` | The filing's index page |
| `document`, `exhibit`, `exhibit_description` | For a document: its file name, its type (`EX-99.1`) and description |
| `tickers`, `state_of_incorporation`, `lei` | For a company |
| `release`, `document`, `federal_register` | For an SRO release: its number, the file, and the number of the Federal Register document it joined, whose facts the card also carries (a `Legislation` card; a `Report` named "Release No. {release}" until the Federal Register publishes it) |

## Examples

- `https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.htm` (an S-1)
- `https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/ea029862901ex99-1.htm` (an 8-K exhibit)
- `https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457` (a company)
- `https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268.pdf` (an SRO release)
