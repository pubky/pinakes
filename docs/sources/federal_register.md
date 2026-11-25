# Federal Register

Pinakes reads documents published in the US Federal Register (rules, proposed rules, notices and
presidential documents) from the Federal Register API, by document number. No key is needed. The
HTML pages are behind a bot wall, and Pinakes never reads them.

| | |
| --- | --- |
| Source | `federal_register`, official source |
| Links | `fr:{number}`; `federalregister.gov/documents/{yyyy}/{mm}/{dd}/{number}/…`, `/d/{number}`, `/documents/{number}`, `/api/v1/documents/{number}[.json]` |
| Fetches | `GET https://www.federalregister.gov/api/v1/documents/{number}.json` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | `fr:{number}`, from the `federal_register` also-on source |
| Credit | None |
| Switch | `[sources.federal_register] enabled`, on by default; the also-on half has its own switch, `[also_on.sources.federal_register] enabled`, on by default |

## Links

Every address of one document is one resource. The root is `fr:{number}`, and these links are
[aliases](../api.md#aliases) of it, on `www.federalregister.gov` or `federalregister.gov`:

- `https://www.federalregister.gov/documents/{yyyy}/{mm}/{dd}/{number}/{slug}`, with any slug or
  none;
- `https://www.federalregister.gov/d/{number}`;
- `https://www.federalregister.gov/documents/{number}`;
- `https://www.federalregister.gov/api/v1/documents/{number}` and `….json`.

A document number has two or three parts joined by `-`: a first part of one to four letters or
digits, then parts of one to six digits each, for example `2026-18291`, `E9-12345` or
`R1-2026-00123`. The root holds the number in upper case, so `fr:e9-12345` is an alias of
`fr:E9-12345`. An `fr:` root that is not a valid number is `missing` at once and never fetched.

Not read here: search pages, agency pages, dated addresses without a day
(`/documents/{yyyy}/{mm}/{number}`), and other hosts. GovInfo and SEC copies of a document are
other resources; the [also-on half](#also-on) gives them the document's key.

## What it fetches

One request per read: `GET https://www.federalregister.gov/api/v1/documents/{number}.json`. A
number the API doesn't know (404) is `missing`. A record for another document number, or one
without a publication date, title or type, is an error. A rate limit (429), a timeout (408) or a
server error is retried after at least a minute.

## Card

Every card is `Legislation`.

- `site` is "Federal Register", `name` the title.
- `description` is the abstract, else the action line, else empty.
- `page_url` is the document's page as the record names it, when that page is a
  federalregister.gov address of the same document; otherwise `https://www.federalregister.gov/d/{number}`.
  An `fr:` card opens that page ([`open_in`](../api.md)).

| Fact | Meaning |
| --- | --- |
| `document_type` | Rule, Proposed Rule, Notice or Presidential Document |
| `agencies` | The agencies, by name |
| `citation` | The Federal Register citation, for example `91 FR 57392` |
| `publication_date` | `YYYY-MM-DD` |
| `docket_ids` | The docket identifiers the document lists, for example `Release No. 34-106268` |
| `regulation_id_numbers` | Regulation identifier numbers (RINs) |
| `comments_close_on` | When the comment period closes, `YYYY-MM-DD`, when the document has one |
| `effective_on` | When the document takes effect, `YYYY-MM-DD`, when the document has one |
| `cfr_references` | The Code of Federal Regulations parts it touches, for example `14 CFR 39` |
| `page_count` | Pages in the printed edition |
| `page_url` | The document's page on federalregister.gov |

## Keys and groups

The official card emits no key of its own. The `federal_register` also-on source records the key
`fr:{number}` (see [Also on](#also-on)). The namespace names one object, so a
[group](../api.md#groups) holds at most one `fr:` value, and a search with `key=fr:{number}` finds
every link that carries it. The citation, docket IDs and RINs are facts only and join nothing.

## Also on

The `federal_register` also-on source runs on three kinds of link. Each read is the same API
request, through the shared discovery cache (an hour). `via` is `federalregister`. Its rows live
30 days and expire after twice that.

### The document

On an `fr:{number}` card it records the key `fr:{number}` and adds derived links from the API
record:

| Site | Link | `kind` |
| --- | --- | --- |
| GovInfo | The PDF the record names, only when it is `https://www.govinfo.gov/content/pkg/FR-{publication_date}/pdf/{number}.pdf` | `pdf` |
| GovInfo | `https://www.govinfo.gov/app/details/FR-{publication_date}/{number}` | |
| Regulations.gov | `https://www.regulations.gov/document/{id}`, when the record names a regulations.gov document | |
| Federal Register public inspection | The public-inspection PDF on `public-inspection.federalregister.gov`, without its query | `pdf` |

A number the API doesn't know gets no key. When the API fails, the key is still recorded and the
run comes back after its backoff.

### A GovInfo copy

A GovInfo copy someone tags gets the key `fr:{number}` of the document it holds:

- `https://www.govinfo.gov/content/pkg/FR-{yyyy-mm-dd}/pdf/{number}.pdf`;
- `https://www.govinfo.gov/app/details/FR-{yyyy-mm-dd}/{number}`.

The key is recorded only when the day in the package name is the document's `publication_date` in
the API record. An address with another day gets no key.

### An SEC release

The SEC's PDF of a self-regulatory organization's rule-change release,
`https://www.sec.gov/files/rules/sro/{org}/{year}/{release}.pdf` (for example `34-106268.pdf`),
gets the key of the one Federal Register document whose docket IDs name `Release No. {release}`
exactly:

```
GET https://www.federalregister.gov/api/v1/documents.json?conditions[docket_id]={release}&fields[]=document_number&fields[]=docket_ids&per_page=20
```

The search matches loosely, so Pinakes checks each result. When no document, or more than one,
names the release exactly, the PDF gets no key. An exhibit (`{release}-{exhibit}.pdf`) is part of
the filing, not a copy of the document, and is not read.

## Credit

The card credits nothing. Federal Register documents are works of the US government, in the
public domain.

## Turn it off

```toml
[sources.federal_register]
enabled = false

[also_on.sources.federal_register]
enabled = false
```

The two switches are independent. With the first off, federalregister.gov links get ordinary web
cards, and `fr:` roots wait until the source is on again. With the second off, cards list no
GovInfo, regulations.gov or public-inspection links and carry no `fr:` key: after the restart the
stored rows stop showing.

## Examples

- `fr:2026-18291` (a notice)
- `https://www.federalregister.gov/documents/2026/09/09/2026-18291/self-regulatory-organizations-nasdaq-texas-llc-notice-of-filing-and-order-granting-accelerated`
