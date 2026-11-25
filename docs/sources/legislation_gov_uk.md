# legislation.gov.uk

Pinakes reads UK legislation (Acts of the UK Parliament, statutory instruments, devolved
legislation and retained EU law) from legislation.gov.uk's XML data for each item, and for each
provision. No key is needed.

| | |
| --- | --- |
| Source | `legislation_gov_uk`, official source |
| Links | `legislation.gov.uk[/id]/{type}/{year}/{number}[/{provision}][/{version}]`, with `/contents`, `/introduction`, `/welsh`, `/data.xml` and similar |
| Fetches | `GET https://www.legislation.gov.uk/{type}/{year}/{number}/introduction[/{version}]/data.xml`, or a provision's own `…/{provision}[/{version}]/data.xml` |
| Credentials | None |
| Rate | 1 request per second on `www.legislation.gov.uk` (the site allows 1,500 per 5 minutes and recommends a gentler pace) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | The Open Government Licence line, in `attribution`; retained EU law adds the European Union's |
| Switch | `[sources.legislation_gov_uk] enabled`, on by default |

## Links

An item is named by its type, year and number, for example `ukpga/2018/12` (the Data Protection Act
2018) or `uksi/2019/419`. Its root is `https://www.legislation.gov.uk/{type}/{year}/{number}`.

These spellings, on `www.legislation.gov.uk` or `legislation.gov.uk`, are
[aliases](../api.md#aliases) of the root:

- `/id/{type}/{year}/{number}`;
- the table of contents, `…/contents`, and the introduction, `…/introduction`;
- a language, `…/welsh` or `…/english`;
- a data format, such as `…/data.xml`.

The type is two to eight lowercase letters, the year four digits and the number a positive number
of up to six digits.

### Provisions and versions

These are resources of their own, each with the same kinds of alias as the item:

- A provision: a section, article, regulation, rule, schedule, part or chapter, and up to three
  more levels inside it (a paragraph, section, article, regulation, rule, part or chapter), for
  example `/ukpga/2018/12/section/1` or `/ukpga/2018/12/schedule/2/paragraph/3`. Each number is up
  to 12 letters and digits.
- A version of the item or of a provision: as enacted, made, adopted, created or prospective
  (`/enacted`, `/made`, `/adopted`, `/created`, `/prospective`), or the text at a date
  (`/2020-01-31`). A date written another way (`/2020-1-31`, `/+2020-01-31`) is an alias of the
  `YYYY-MM-DD` root, and `…/contents/2020-01-31` is an alias of `…/2020-01-31`.

### Not read here

- The explanatory notes, memoranda, impact assessments and other documents published beside an
  item (`/notes`, `/memorandum`, `/impacts`, …), and its `/body`.
- Items numbered by regnal year (`/ukpga/Vict/63-64/51`).
- A type and year without a number, a provision kind without a number, and search pages.

Retained EU law (`/eur/…`) is the UK's amended copy, a different object from the act on EUR-Lex.

## What it fetches

One request per read, for the version the link names (or the current text):

| Link | Request |
| --- | --- |
| Item | `GET https://www.legislation.gov.uk/{type}/{year}/{number}/introduction[/{version}]/data.xml` |
| Provision | `GET https://www.legislation.gov.uk/{type}/{year}/{number}/{provision}[/{version}]/data.xml` |

A provision's own data carries the item's metadata, so a provision's card is the item's card for
the same version. A provision whose data is larger than 1 MB (a long part) is not read: a second
request reads the item's introduction instead.

- An item or provision the site doesn't know (400 or 404) is `missing`.
- Data for another item, or without metadata or a title, is an error.
- A rate limit (429) is retried after at least the time its `Retry-After` header names (up to an
  hour), or 5 minutes when it names none.
- A timeout (408) or a server error is retried after at least a minute. Any other status is an
  error.

## Card

Every card is `Legislation`.

- `name` is the item's title; for a provision, the title and the provision, for example "Data
  Protection Act 2018, section 1".
- `description` is the item's description (its long title).
- `attribution` is "Contains public sector information licensed under the Open Government Licence
  v3.0.". For retained EU law (category `euretained`) it adds "Material derived from the European
  Institutions © European Union, 1998-2019 and re-used under the terms of the Commission Decision
  2011/833/EU."

| Fact | Meaning |
| --- | --- |
| `legislation_type` | For example `UnitedKingdomPublicGeneralAct`, `UnitedKingdomStatutoryInstrument` |
| `category` | For example `primary`, `secondary` or `euretained` |
| `year` | As in its address |
| `number` | As in its address |
| `enacted` | When the item was enacted, `YYYY-MM-DD` |
| `made` | When the item was made, `YYYY-MM-DD` (statutory instruments) |
| `provision` | For a provision: for example `section 1`, `schedule 2 paragraph 3` |
| `version` | For a version: for example `enacted`, `2020-01-31` |
| `page_url` | Its root on legislation.gov.uk |

## Keys and groups

None. An item, its provisions and its versions are separate resources and don't join one another.

## Credit

The `attribution` line credits the data under the Open Government Licence v3.0, the licence
legislation.gov.uk states for it. Retained EU law also credits the European Union.

## Turn it off

```toml
[sources.legislation_gov_uk]
enabled = false
```

legislation.gov.uk links then get ordinary web cards.

## Examples

- `https://www.legislation.gov.uk/ukpga/2018/12/contents` (the Data Protection Act 2018)
- `https://www.legislation.gov.uk/uksi/2019/419/made` (a version)
