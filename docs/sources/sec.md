# SEC EDGAR

Pinakes reads filings, filed documents and companies on EDGAR, the US Securities and Exchange
Commission's filing system, and names the SEC's PDFs of rule-change releases. A filing's card comes
from its header, never from its filed documents. No key is needed, but EDGAR links wait until the
operator sets a contact address.

| | |
| --- | --- |
| Source | `sec`, official source |
| Links | `sec.gov/Archives/edgar/data/{cik}/{accession}/…`; `sec.gov/ix?doc=…`; `sec.gov/cgi-bin/browse-edgar?…CIK=…`, `sec.gov/edgar/browse/?CIK=…`, `data.sec.gov/submissions/CIK{cik}.json`; `sec.gov/files/rules/sro/{org}/{year}/{release}.pdf` |
| Fetches | `GET https://www.sec.gov/Archives/edgar/data/{cik}/{folder}/{accession}.hdr.sgml` (and the filing's index page for a document); `GET https://data.sec.gov/submissions/CIK{cik}.json`; nothing for a release |
| Credentials | None. `http.contact` must be set: without it, EDGAR links wait and are tried again every hour |
| Rate | At most 5 requests per second on `www.sec.gov` and 2 on `data.sec.gov` (EDGAR allows 10 in total), and never above `limits.host_requests_per_second` (1 by default) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Report` (filings, documents, releases), `Corporation` (companies) |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.sec] enabled`, on by default |

## Links

The source reads four kinds of object on `www.sec.gov` or `sec.gov`, and companies on
`data.sec.gov` too. A `sec.gov` link is read from `www.sec.gov`.

### Filings

A filing is named by the CIK in its address and its accession number (for example
`0001104659-26-000959`, 18 digits in the folder name). The root is the filing's folder,
`https://www.sec.gov/Archives/edgar/data/{cik}/{accession, 18 digits}/`. These links are
[aliases](../api.md#aliases) of it:

- the folder without its trailing slash;
- `{accession}-index.htm`, `{accession}-index.html` and `{accession}-index-headers.html` in it;
- the full text `{accession}.txt` and the header `{accession}.hdr.sgml` in it;
- `…/data/{cik}/{accession}.txt`, `…/data/{cik}/{accession}-index.htm` and
  `…/data/{cik}/{accession}-index.html` beside it.

### Documents

A document of a filing, the main form or an exhibit (`…/data/{cik}/{accession}/{file}`), is its own
resource. Its inline viewer addresses, `https://www.sec.gov/ix?doc=/Archives/edgar/data/…` (also
`/ix.xhtml?doc=` and `/cgi-bin/viewer?doc=`), are aliases of it. A file name is letters, digits,
`-`, `_` and `.`, at most 200 characters, not starting with a dot.

### Co-registrants

A filing can be listed under several CIKs: co-registrants, or the issuer and the reporting owner of
an ownership filing. After the first read, a filing under another CIK becomes an alias of the
primary filer's folder, and a document of it an alias of the same document in that folder.

The primary filer is, in this order: the issuer of an ownership filing (Forms 3, 4 and 5) or the
issuing entity of an asset-backed filing; else the first party in the header's `FILER`,
`SUBJECT-COMPANY` or `FILED-BY` sections; else, for a header that names only reporting owners, the
first owner.

### Companies

A company is named by its CIK. The root is
`https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={cik, ten digits}`. These links are
aliases of it:

- `https://www.sec.gov/cgi-bin/browse-edgar?…CIK={cik}…`, with other query parameters such as
  `type=8-K`;
- `https://www.sec.gov/edgar/browse/?CIK={cik}`;
- `https://data.sec.gov/submissions/CIK{cik, ten digits}.json`.

A ticker in place of the CIK is not read.

### Rule-change releases

The SEC's PDF of a self-regulatory organization's rule-change release,
`https://www.sec.gov/files/rules/sro/{org}/{year}/{release}.pdf` (for example `34-106268.pdf`), and
each exhibit of it, `{release}-{exhibit}.pdf`, is its own resource with no root. A release number
is two digits, a hyphen and four to seven digits.

Not read here: CIK folders without an accession, press releases and other sec.gov pages.

## What it fetches

### A filing or document

1. `GET https://www.sec.gov/Archives/edgar/data/{cik}/{folder}/{accession}.hdr.sgml`: the filing's
   SGML header. The filed documents themselves are never fetched.
2. For a document only: `GET …/{folder}/{accession}-index.htm`, the filing's index page, for the
   document's type and description. When that page fails, the card has neither.

A header EDGAR doesn't have (404) is `missing`. A header for another accession number, or one
without an accession number, a form type or a party, is an error.

### A company

`GET https://data.sec.gov/submissions/CIK{cik, ten digits}.json`. A CIK EDGAR doesn't know (404) is
`missing`. An answer for another CIK is an error.

### A rule-change release

Nothing. The PDF is never fetched and the card comes from the link alone, so a release link doesn't
wait for `http.contact`.

### Errors and waits

EDGAR asks for a User-Agent with an email address. While `http.contact` is empty, filing, document
and company links are not sent: each read waits an hour and is tried again. A rate limit (429), a
timeout (408) or a server error is retried after at least a minute. Any other status is an error.

## Card

### Filings and documents

A filing is a `Report` named "Form {form} · {primary filer}", for example "Form 8-K · Twenty One
Capital, Inc.". Its `description` joins, with " · ", the parts the header has:

- "Filed {date}";
- "period {date}";
- "items {items}";
- "reporting owner {owners}", for an ownership filing whose primary filer is not its owner.

A document's card is the filing's card with its own `document`, `exhibit` and
`exhibit_description` facts.

### Companies

A company is a `Corporation` with its EDGAR name. `description` is its industry (EDGAR's SIC
description).

### Rule-change releases

A release is a `Report` named "Release No. {release}", with `site` "www.sec.gov".

### Facts

| Fact | Meaning |
| --- | --- |
| `form` | The form type, for example `S-1`, `8-K`, `10-Q` |
| `accession` | The accession number, for example `0001104659-26-000959` |
| `filing_date` | `YYYY-MM-DD` |
| `period` | The period of report, `YYYY-MM-DD` |
| `items` | The 8-K items, for example `5.02; 8.01; 9.01` |
| `company` | The filers, primary first |
| `cik` | The primary filer's CIK; on a company card, the company's |
| `file_number` | The SEC file number, for example `333-292586` (for an ownership filing, from the owner's section when the issuer's has none) |
| `reporting_owner` | The reporting owners of an ownership filing |
| `depositor` | An asset-backed filing's depositor, by name, when the header has one |
| `depositor_cik` | An asset-backed filing's depositor's CIK |
| `asset_class` | An asset-backed filing's asset class, for example `Auto loans` |
| `page_url` | The filing's index page, `…/{folder}/{accession}-index.htm` |
| `document` | On a document: its file name. On a release: the PDF's file name |
| `exhibit` | On a document: its type, for example `EX-99.1` |
| `exhibit_description` | On a document: its description in the filing's index |
| `tickers` | On a company: its tickers |
| `state_of_incorporation` | On a company: its state of incorporation |
| `lei` | On a company: its LEI, when EDGAR has one |
| `release` | On a release: its number, for example `34-106268` |

## Keys and groups

The card emits no keys. CIKs, accession numbers, file numbers and LEIs are facts and join nothing.

A release PDF gets a key from another source: the
`federal_register` also-on source gives it the `fr:{number}` key of
the Federal Register document whose docket IDs name `Release No. {release}`. The release then joins
that document's [group](../api.md#groups). An exhibit gets no key.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.sec]
enabled = false
```

EDGAR, `data.sec.gov` and release links then get ordinary web cards.

## Examples

- `https://www.sec.gov/Archives/edgar/data/2103612/000110465926000959/0001104659-26-000959-index.htm` (an S-1)
- `https://www.sec.gov/Archives/edgar/data/2070457/000121390026079805/ea029862901ex99-1.htm` (an 8-K exhibit)
- `https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK=0002070457` (a company)
- `https://www.sec.gov/files/rules/sro/nasdaqtx/2026/34-106268.pdf` (an SRO release)
