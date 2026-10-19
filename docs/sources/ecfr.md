# eCFR

The Electronic Code of Federal Regulations: sections and parts of the US Code of Federal
Regulations, read from the eCFR versioner API. The eCFR pages are behind a bot wall and are never
read. Public domain; no key; one request per second.

## Links

A **section** is named by its title and section number, a **part** by its title and part
number. Every eCFR address of one, current or at a date (`/on/{YYYY-MM-DD}/…`), with or without
the chapter, subchapter, subpart and subject-group segments, is an alias of its current address:

- `https://www.ecfr.gov/current/title-{title}/section-{section}` (the root of a section), e.g.
  `…/title-17/chapter-II/part-240/subpart-A/subject-group-…/section-240.10b-5`;
- `https://www.ecfr.gov/current/title-{title}/part-{part}` (the root of a part), e.g.
  `…/title-14/chapter-I/subchapter-C/part-39`.

A section and the part named beside it must agree (`part-240/section-240.10b-5`). The legacy
`/cgi-bin/text-idx` addresses are not read.

## What it reads

`GET https://www.ecfr.gov/api/versioner/v1/ancestry/current/title-{title}.json?section={section}`
(or `?part={part}`): the object's heading and every level above it, about 1 KB. A section or part
eCFR does not know is `missing`.

## Card

`Legislation`, named by its label and heading ("§ 240.10b-5 Employment of manipulative and
deceptive devices.", "Part 39 Airworthiness Directives"), described by its title, chapter and
part.

| Fact | Meaning |
| --- | --- |
| `citation` | E.g. `17 CFR 240.10b-5`, `14 CFR 39` |
| `cfr_title`, `chapter`, `cfr_part` | The headings of its title, chapter and part |
| `reserved` | `true` for a reserved (empty) section or part |
| `page_url` | Its current eCFR address |

## Examples

- `https://www.ecfr.gov/current/title-17/section-240.10b-5`
- `https://www.ecfr.gov/current/title-14/chapter-I/subchapter-C/part-39`
