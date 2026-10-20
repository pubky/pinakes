# EUR-Lex

EU law (regulations, directives, decisions, corrigenda and consolidated texts) read through the
EU Publications Office's SPARQL endpoint, by CELEX number, ELI, Official Journal act id or Cellar
id. The EUR-Lex pages are behind a bot challenge and are never read. No key; one request per
second. Reuse with acknowledgement of the source (Commission Decision 2011/833/EU): the card
carries "© European Union, https://eur-lex.europa.eu".

## Links

An act's CELEX number is its key, and `https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:{celex}`
is its root. Every `legal-content` address with `uri=CELEX:{celex}` (any language, `TXT`,
`ALL`, `HTML` or `PDF`, any other parameters) is an alias of it. These addresses are read by
their own key and become aliases of the act's root after their first fetch:

- an ELI: `https://eur-lex.europa.eu/eli/{type}/{year}/{number}/oj[/{language}…]` and
  `http://data.europa.eu/eli/{type}/{year}/{number}/oj`;
- an Official Journal act id from October 2023 on: `legal-content/…?uri=OJ:L_{yyyy}{nnnnn}`;
- a Cellar work: `https://publications.europa.eu/resource/cellar/{uuid}[…]` and
  `https://op.europa.eu/{lang}/publication-detail/-/publication/{uuid}`.

Official Journal issue tables of contents and older Official Journal references name more than
one act, or no act exactly, and are not read. Corrigenda and consolidated texts have their own
CELEX numbers and are their own resources.

## What it reads

`GET https://publications.europa.eu/webapi/rdf/sparql?query=…` for the work with that key: its
CELEX number, ELI, resource types, date, in-force flag and English title (about 1 KB). A key with
no work is `missing`.

## Card

`Legislation`, named by its English title and described by its CELEX number and date.

| Fact | Meaning |
| --- | --- |
| `celex` | The CELEX number, e.g. `32016R0679` |
| `eli` | The ELI, e.g. `http://data.europa.eu/eli/reg/2016/679/oj` |
| `resource_type` | E.g. `REG`, `DIR`, `CORRIGENDUM`, `CONS_TEXT` |
| `document_date` | `YYYY-MM-DD` |
| `in_force` | `true` or `false`, when known |
| `page_url` | The act's page on EUR-Lex |

## Examples

- `https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:32016R0679` (the GDPR)
- `https://eur-lex.europa.eu/eli/reg/2023/1114/oj` (MiCA)
