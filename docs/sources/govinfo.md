# GovInfo

The US Government Publishing Office's GovInfo repository: public and private laws, bills, US Code
sections and laws as printed in the Statutes at Large, read from their MODS metadata records.
Public domain; no key; one request per second. GovInfo's copies of Federal Register documents and
of the Code of Federal Regulations are other editions of what the
`federal_register` and `ecfr` sources read, and are not read
here.

## Links

- A **law**, e.g. Public Law 117-58: `https://www.govinfo.gov/app/details/PLAW-117publ58` is the
  root; its PDF, HTML and XML copies under `/content/pkg/…`, its MODS record, the link service's
  `/link/plaw/{congress}/{public|private}/{number}` and the legacy `www.gpo.gov/fdsys/pkg/…`
  addresses are aliases.
- A **bill version**, e.g. `BILLS-117hr3684enr`: `/app/details/{package}` is the root, with the
  same kinds of aliases and `/link/bills/{congress}/{type}/{number}?billversion={version}`. A link
  service address without a version names no one version and is not read.
- A **US Code section**: `https://www.govinfo.gov/link/uscode/{title}/{section}` is the root and
  shows the current edition. One edition's copy,
  `/app/details/USCODE-{year}-title{t}/USCODE-{year}-title{t}-…-sec{s}`, is its own resource,
  with its content and MODS record as aliases; the cards share the citation. A link service
  address with `?year=` names an edition without its granule and is not read.
- A **Statutes at Large** entry: `/app/details/STATUTE-{volume}/STATUTE-{volume}-Pg{page}` is the
  root; `/link/statute/{volume}/{page}` is an alias.

## What it reads

The MODS record: `/metadata/pkg/{package}/mods.xml` for a law or bill (hundreds of KB for a large
law), the section's record through `/link/uscode/{title}/{section}?link-type=mods` (an edition's
copy: `/metadata/granule/{package}/{granule}/mods.xml`; a US Code title's own record is tens of
MB and never read), and the page's record for a Statutes at Large entry. A record GovInfo does
not have is `missing`; a record still being prepared (503) is retried. A record larger than the
1 MB body limit gives a card named by the link itself ("Public Law 117-58", "H.R. 3684 (enr),
117th Congress").

## Card

`Legislation`. A law or Statutes at Large entry is named by its short title (else its title) and
described by its full title; a bill by its short title; a US Code section by its heading,
described by its citation and the title's name ("26 U.S.C. 501 · INTERNAL REVENUE CODE").

| Fact | Meaning |
| --- | --- |
| `public_law` | E.g. `Public Law 117-58` |
| `congress` | E.g. `117` |
| `enacted` | The law's date (`YYYY-MM-DD`) |
| `bill` | E.g. `H.R. 3684` |
| `bill_version`, `origin_chamber`, `current_chamber` | For a bill version, e.g. `enr`, `HOUSE`, `JOINT` |
| `date_issued` | The bill version's or the US Code edition's date |
| `edition` | For one edition's copy of a US Code section: its year |
| `citation` | E.g. `26 U.S.C. 501`, `135 Stat. 429` |
| `code_title` | The US Code title's name |
| `page_url` | Its page on GovInfo |

## Examples

- `https://www.govinfo.gov/app/details/PLAW-117publ58` (the Infrastructure Investment and Jobs Act)
- `https://www.govinfo.gov/link/uscode/26/501`
