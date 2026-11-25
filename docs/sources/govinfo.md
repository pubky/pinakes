# GovInfo

Pinakes reads laws, bill versions, US Code sections and Statutes at Large entries from GovInfo, the
US Government Publishing Office's repository, through their MODS metadata records. No key is
needed.

| | |
| --- | --- |
| Source | `govinfo`, official source |
| Links | `govinfo.gov/app/details/…`, `/content/pkg/…`, `/metadata/…`, `/link/{plaw,bills,uscode,statute}/…`; legacy `gpo.gov/fdsys/…` |
| Fetches | One MODS record per read, for example `GET https://www.govinfo.gov/metadata/pkg/{package}/mods.xml` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.govinfo] enabled`, on by default |

## Links

The source reads four kinds of object, on `www.govinfo.gov` or `govinfo.gov`, and on the legacy
`www.gpo.gov/fdsys/pkg/…` and `/fdsys/granule/…` addresses. Every other spelling of an object is an
[alias](../api.md#aliases) of its root.

### Laws

A public or private law, for example Public Law 117-58 (package `PLAW-117publ58`, or `…pvtl…` for a
private law).

- Root: `https://www.govinfo.gov/app/details/PLAW-117publ58`.
- Aliases: its PDF, HTML and XML copies under `/content/pkg/PLAW-117publ58/…`, its MODS record
  `/metadata/pkg/PLAW-117publ58/mods.xml`, the link service's
  `/link/plaw/{congress}/{public|private}/{number}`, and the legacy `gpo.gov/fdsys/pkg/…`
  addresses.

### Bill versions

One version of a bill, for example `BILLS-117hr3684enr`: the congress, the bill type (`hr`, `s`,
`hjres`, `sjres`, `hconres`, `sconres`, `hres`, `sres`), the number and the version code.

- Root: `https://www.govinfo.gov/app/details/{package}`.
- Aliases: the same kinds of address as for a law, and
  `/link/bills/{congress}/{type}/{number}?billversion={version}`.

A link service address without `billversion` names no single version and is not read.

### US Code sections

- Root: `https://www.govinfo.gov/link/uscode/{title}/{section}`, for whatever edition is current.
  The same address with a query other than `year`, such as `?link-type=details`, is an alias.
- One edition's copy,
  `https://www.govinfo.gov/app/details/USCODE-{year}-title{t}/USCODE-{year}-title{t}-…-sec{s}`, is
  its own resource, not an alias of the current section. Its content and MODS record are its
  aliases.

A link service address with `?year=` names an edition without its granule and is not read. Neither
is a US Code title's package on its own (`/app/details/USCODE-2024-title26`).

### Statutes at Large entries

A law as printed in the Statutes at Large, by volume and first page.

- Root: `https://www.govinfo.gov/app/details/STATUTE-{volume}/STATUTE-{volume}-Pg{page}`.
- Alias: `/link/statute/{volume}/{page}`.

### Not read here

GovInfo's copies of Federal Register documents (`FR-…`) and of the Code of Federal Regulations
(`CFR-…`) are other editions of what the `federal_register` and
`ecfr` sources read. This source doesn't read them, and the
`federal_register` also-on source gives a GovInfo Federal Register
copy its document's key. Collection pages and other packages are not read either.

## What it fetches

One request per read, for the object's MODS record:

| Object | Request |
| --- | --- |
| Law, bill version | `GET https://www.govinfo.gov/metadata/pkg/{package}/mods.xml` |
| US Code section, current | `GET https://www.govinfo.gov/link/uscode/{title}/{section}?link-type=mods` |
| US Code section, one edition | `GET https://www.govinfo.gov/metadata/granule/{package}/{granule}/mods.xml` |
| Statutes at Large entry | `GET https://www.govinfo.gov/metadata/granule/STATUTE-{volume}/STATUTE-{volume}-Pg{page}/mods.xml` |

A US Code section is always read through its granule's record, never the title's package record,
which runs to tens of megabytes.

- A record GovInfo doesn't have (400, 404, or a redirect to its `/error` page) is `missing`.
- A record for another package, section, edition or page is an error.
- A rate limit (429), a timeout (408) or a server error, such as a 503 for a record GovInfo is
  still preparing, is retried after at least a minute.
- A record larger than 1 MB is not read. The card then holds only what the link names: its
  citation as `name` and `citation` ("Public Law 117-58", "H.R. 3684 (enr), 117th Congress",
  "26 U.S.C. 501", "135 Stat. 429") and its `page_url`.

## Card

Every card is `Legislation`.

| Object | `name` | `description` |
| --- | --- | --- |
| Law | Its short title, else its title | Its full title, when it has a short title |
| Bill version | The measure's short title, else its title | Its full title, when it has a short title |
| US Code section | The section's heading | Its citation and the title's name, for example "26 U.S.C. 501 · INTERNAL REVENUE CODE" |
| Statutes at Large entry | Its short title, else its title | Its full title, when it has a short title |

A record with no title is an error.

| Fact | Meaning |
| --- | --- |
| `public_law` | On a law or Statutes at Large entry: for example `Public Law 117-58` |
| `congress` | On a law or bill version: the congress, for example `117` |
| `enacted` | On a law or Statutes at Large entry: the law's date, `YYYY-MM-DD` |
| `bill` | On a law, its primary bill; on a bill version, the bill itself; for example `H.R. 3684` |
| `bill_version` | On a bill version: its code, for example `enr` |
| `origin_chamber`, `current_chamber` | On a bill version: for example `HOUSE`, `JOINT` |
| `date_issued` | The bill version's date, or the US Code edition's date |
| `edition` | On one edition's copy of a US Code section: its year |
| `citation` | On a US Code section or Statutes at Large entry: for example `26 U.S.C. 501`, `135 Stat. 429` |
| `code_title` | On a US Code section: the US Code title's name |
| `page_url` | Its page on GovInfo (for the current US Code section, `…/link/uscode/{title}/{section}?link-type=details`) |

## Keys and groups

None. A US Code edition's copy and the current section share a `citation`, but the citation is a
fact and joins nothing.

## Credit

The card credits nothing. GovInfo's laws, bills and US Code are works of the US government, in the
public domain.

## Turn it off

```toml
[sources.govinfo]
enabled = false
```

GovInfo and `gpo.gov` links then get ordinary web cards.

## Examples

- `https://www.govinfo.gov/app/details/PLAW-117publ58` (the Infrastructure Investment and Jobs Act)
- `https://www.govinfo.gov/link/uscode/26/501`
