# legislation.gov.uk

UK legislation (Acts of the UK Parliament, statutory instruments, devolved legislation and
retained EU law) from legislation.gov.uk's data for each item. No key; the site allows 1,500
requests per 5 minutes and asks for a gentle pace (one per second here). Open Government Licence
v3.0: the card carries "Contains public sector information licensed under the Open Government
Licence v3.0.", and retained EU law also the European Union's credit.

## Links

An item is named by its type, year and number. `https://www.legislation.gov.uk/{type}/{year}/{number}`
is the root; every other address of the item is an alias of it: `/id/…`, the table of contents,
the introduction, a part, chapter, section, article or schedule, a version (`/enacted`, `/made`,
a date), a language (`/welsh`) and a data format (`/data.xml`). A section is read as its item.
Items numbered by regnal year (`/ukpga/Vict/63-64/51`) are not read.

Retained EU law (`/eur/…`) is the UK's amended copy, a different object from the act on EUR-Lex.

## What it reads

`GET https://www.legislation.gov.uk/{type}/{year}/{number}/introduction/data.xml` (tens of KB). An
item the site does not know is `missing` (it answers 400).

## Card

`Legislation`, named by its title and described by its long title.

| Fact | Meaning |
| --- | --- |
| `legislation_type` | E.g. `UnitedKingdomPublicGeneralAct`, `UnitedKingdomStatutoryInstrument`, `EuropeanUnionRegulation` |
| `category` | `primary`, `secondary` or `euretained` |
| `year`, `number` | As in its address |
| `enacted`, `made` | `YYYY-MM-DD` (made for statutory instruments) |
| `page_url` | The item on legislation.gov.uk |

## Examples

- `https://www.legislation.gov.uk/ukpga/2018/12/contents` (the Data Protection Act 2018)
- `https://www.legislation.gov.uk/uksi/2019/419/made`
