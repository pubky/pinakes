# legislation.gov.uk

UK legislation (Acts of the UK Parliament, statutory instruments, devolved legislation and
retained EU law) from legislation.gov.uk's data for each item. No key; the site allows 1,500
requests per 5 minutes and asks for a gentle pace (one per second here). Open Government Licence
v3.0: the card carries "Contains public sector information licensed under the Open Government
Licence v3.0.", and retained EU law also the European Union's credit.

## Links

An item is named by its type, year and number. `https://www.legislation.gov.uk/{type}/{year}/{number}`
is the root; `/id/…`, the table of contents (`/contents`), the introduction, a language
(`/welsh`) and a data format (`/data.xml`) are aliases of it.

These are their own resources, each with the same kinds of aliases:

- a **provision**: a section, article, regulation, rule, schedule, part or chapter, and the
  paragraphs in it, e.g. `/ukpga/2018/12/section/1` or `/ukpga/2018/12/schedule/2/paragraph/3`;
- a **version** of the item or of a provision: as enacted, made, adopted, created or
  prospective (`/enacted`, `/made`, …), or the text at a date (`/2020-01-31`; a date written
  another way, `/2020-1-31`, is an alias of it).

The explanatory notes, memoranda, impact assessments and other documents published beside an
item (`/notes`, `/memorandum`, `/impacts`, …) are not read, nor items numbered by regnal year
(`/ukpga/Vict/63-64/51`).

Retained EU law (`/eur/…`) is the UK's amended copy, a different object from the act on EUR-Lex.

## What it reads

`GET https://www.legislation.gov.uk/{type}/{year}/{number}/introduction[/{version}]/data.xml`
(tens of KB). A provision is read from its own data,
`…/{number}/{provision}[/{version}]/data.xml`, which carries the item's metadata: its card is the
item's, for the same version. A provision over the 1 MB body limit (a long part) takes its card
from the introduction instead. An item or provision the site does not know is `missing` (it
answers 400 or 404).

## Card

`Legislation`, named by its title (with the provision: "Data Protection Act 2018, section 1")
and described by its long title.

| Fact | Meaning |
| --- | --- |
| `legislation_type` | E.g. `UnitedKingdomPublicGeneralAct`, `UnitedKingdomStatutoryInstrument`, `EuropeanUnionRegulation` |
| `category` | `primary`, `secondary` or `euretained` |
| `year`, `number` | As in its address |
| `enacted`, `made` | `YYYY-MM-DD` (made for statutory instruments) |
| `provision` | For a provision, e.g. `section 1`, `schedule 2 paragraph 3` |
| `version` | For a version, e.g. `enacted`, `2020-01-31` |
| `page_url` | The item on legislation.gov.uk |

## Examples

- `https://www.legislation.gov.uk/ukpga/2018/12/contents` (the Data Protection Act 2018)
- `https://www.legislation.gov.uk/uksi/2019/419/made` (a version)
