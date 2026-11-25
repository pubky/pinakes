# EUR-Lex

Pinakes reads EU law (regulations, directives, decisions, corrigenda and consolidated texts) from
the EU Publications Office's SPARQL endpoint, by CELEX number, ELI, Official Journal act ID or
Cellar ID. No key is needed. The EUR-Lex pages are behind a bot challenge, and Pinakes never reads
them.

| | |
| --- | --- |
| Source | `eur_lex`, official source |
| Links | `eur-lex.europa.eu/legal-content/…?uri=CELEX:{celex}` or `?uri=OJ:{series}_{id}`; `eur-lex.europa.eu/eli/…/oj`, `data.europa.eu/eli/…/oj`; `publications.europa.eu/resource/cellar/{uuid}`, `op.europa.eu/{lang}/publication-detail/-/publication/{uuid}` |
| Fetches | `GET https://publications.europa.eu/webapi/rdf/sparql?query=…` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | "© European Union, https://eur-lex.europa.eu", in `attribution` |
| Switch | `[sources.eur_lex] enabled`, on by default |

## Links

An act's root is its CELEX page,
`https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:{celex}`.

### CELEX addresses

Every `eur-lex.europa.eu/legal-content/…` address with `uri=CELEX:{celex}` is an
[alias](../api.md#aliases) of the root, whatever its language, view (`TXT`, `ALL`, `HTML`, `PDF`)
or other parameters. For example, `…/legal-content/FR/TXT/PDF/?uri=CELEX:32016R0679&from=EN` is an
alias of the GDPR's root.

A CELEX number is 8 to 40 letters, digits and `( ) - . _`, starting with a letter or digit followed
by a four-digit year. Pinakes reads it in upper case. Corrigenda and consolidated texts have their
own CELEX numbers (`32016R0679R(02)`) and are their own resources.

### Other identifiers

These links are read by their own identifier. Each becomes an alias of the act's root after its
first read, once the endpoint has named the act's CELEX number:

- an ELI: `https://eur-lex.europa.eu/eli/{type}/{year}/{number}/oj`, with or without a language
  and further segments after `oj`, and `http://data.europa.eu/eli/{type}/{year}/{number}/oj`;
- an Official Journal act ID, as used from October 2023 on:
  `legal-content/…?uri=OJ:L_{yyyy}{nnnnn}` (or `C_…`, nine digits);
- a Cellar work: `https://publications.europa.eu/resource/cellar/{uuid}[.…]` and
  `https://op.europa.eu/{lang}/publication-detail/-/publication/{uuid}`.

### Not read here

- Official Journal issue tables of contents and older Official Journal references
  (`OJ:L:2016:119:TOC`): they name more than one act, or no act exactly.
- An ELI without `/oj` (`…/eli/reg/2016/679`).
- The EUR-Lex home page and search pages.

## What it fetches

One SPARQL query per read, as a GET:

```
GET https://publications.europa.eu/webapi/rdf/sparql?query=…
```

The query selects the work with the link's CELEX number, ELI, Official Journal ID or Cellar ID, and
asks for its CELEX number, ELI, resource types, document date, in-force flag and English title.

- No work for a CELEX number, ELI or Official Journal ID: the card is `missing`.
- No legal act for a Cellar ID (a report, a study, a book): the source declines the link, and the
  Publications Office page gets a web card.
- More than one work for one identifier, a work without a CELEX number, or a work whose CELEX
  number is not the one asked for: an error.
- A rate limit (429) is retried after at least the time its `Retry-After` header names (up to an
  hour), or 5 minutes when it names none.
- A timeout (408) or a server error is retried after at least a minute. Any other status is an
  error.

## Card

Every card is `Legislation`.

- `name` is the English title, else the CELEX number.
- `description` is "CELEX {celex}", followed by " · {date}" when the act has a document date, for
  example "CELEX 32016R0679 · 2016-04-27".
- `attribution` is "© European Union, https://eur-lex.europa.eu".

| Fact | Meaning |
| --- | --- |
| `celex` | The CELEX number, for example `32016R0679` |
| `eli` | The ELI, for example `http://data.europa.eu/eli/reg/2016/679/oj` |
| `resource_type` | The resource types, for example `REG` or `DIR`; a corrigendum has `CORRIGENDUM` among them |
| `document_date` | `YYYY-MM-DD` |
| `in_force` | `true` or `false`, when the endpoint states it |
| `page_url` | The act's CELEX page on EUR-Lex (its root) |

## Keys and groups

None. Every link of one act becomes an alias of one root, so they share one card. The CELEX number
and ELI are facts and join nothing else.

## Credit

The `attribution` line credits the European Union and EUR-Lex.

## Turn it off

```toml
[sources.eur_lex]
enabled = false
```

EUR-Lex, `data.europa.eu` ELI, Cellar and `op.europa.eu` publication links then get ordinary web
cards.

## Examples

- `https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX:32016R0679` (the GDPR)
- `https://eur-lex.europa.eu/eli/reg/2023/1114/oj` (MiCA)
