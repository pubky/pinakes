# Fedlex

Pinakes reads Swiss federal law in the classified compilation (the Constitution, federal acts,
ordinances, treaties) from Fedlex's SPARQL endpoint (the JOLux model), by ELI. No key is needed.
The Fedlex pages are an application shell without metadata, and Pinakes never reads them.

| | |
| --- | --- |
| Source | `fedlex`, official source |
| Links | `fedlex.admin.ch/eli/cc/{a}/{b}[/…]`, `fedlex.data.admin.ch/eli/cc/{a}/{b}[/…]`; `admin.ch/opc/{lang}/classified-compilation/{dossier}/…` |
| Fetches | `GET https://fedlex.data.admin.ch/sparqlendpoint?query=…` (two queries for an old `admin.ch/opc` address) |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | "Source: Fedlex, www.fedlex.admin.ch", in `attribution` |
| Switch | `[sources.fedlex] enabled`, on by default |

## Links

An act is named by its ELI, `eli/cc/{a}/{b}`, where each part is up to 40 digits, lowercase letters
and `_` (`eli/cc/1999/404`, `eli/cc/24/233_245_233`). Its root is
`https://fedlex.data.admin.ch/eli/cc/{a}/{b}`.

Every address of the act on `www.fedlex.admin.ch`, `fedlex.admin.ch` or `fedlex.data.admin.ch` is
an [alias](../api.md#aliases) of the root, in any language (`/de`, `/fr`, `/it`, `/rm`, `/en`) and
with any article fragment (`#art_5`).

### Dated versions

A dated version, `…/eli/cc/{a}/{b}/{yyyymmdd}[/…]`, is its own resource with the root
`https://fedlex.data.admin.ch/eli/cc/{a}/{b}/{yyyymmdd}`. A date written with a one-digit day
(`2024033`) is an alias of the eight-digit root. A segment in that place that is no valid date
names the act itself.

### Old addresses

An old classified-compilation address,
`https://www.admin.ch/opc/{lang}/classified-compilation/{number}/…`, is read by its eight-digit
number (the act's dossier). It becomes an alias of the act's root after its first read.

### Not read here

Official-compilation addresses (`eli/oc/…`) name a publication, not the act in force. Fedlex's home
and search pages, and old addresses without an eight-digit number, are not read either.

## What it fetches

One SPARQL query per read, as a GET to `https://fedlex.data.admin.ch/sparqlendpoint?query=…`. It
asks for the act's title and abbreviation in each language, its SR number, document date, date of
entry into force, the latest applicable version up to today, its status and its type.

For an old address, a first query finds the act that carries the dossier number and has an in-force
status. When there is no such act, or more than one, the card is `missing`; otherwise the second
query reads that act.

An ELI with no act is `missing`. A rate limit (429), a timeout (408) or a server error is retried
after at least a minute. Any other status is an error.

## Card

Every card is `Legislation`.

- `name` is the German title, else the French, Italian, English or Romansh one.
- `description` joins the SR number, the abbreviation and the status, for example
  "SR 101 · BV · In force".
- `attribution` is "Source: Fedlex, www.fedlex.admin.ch".

A dated version's card is the act's card, with a `version_date` fact and the version's page as
`page_url`.

| Fact | Meaning |
| --- | --- |
| `sr_number` | Its number in the classified compilation, for example `101`, `210` |
| `abbreviation` | The abbreviation in the card's title language, for example `BV`, `ZGB` |
| `english_title` | Its English title, where Fedlex has one |
| `document_date` | The act's date, `YYYY-MM-DD` |
| `entry_into_force` | When it entered into force, `YYYY-MM-DD` |
| `applicable_version` | The date of the latest version applicable today |
| `version_date` | For a dated version: its date, `YYYY-MM-DD` |
| `status` | In English, for example "In force" |
| `document_type` | In English, for example "Federal act" |
| `page_url` | Its page on Fedlex, in German: `https://www.fedlex.admin.ch/{eli}[/{yyyymmdd}]/de` |

## Keys and groups

None. Every address of one act becomes an alias of one root, so they share one card. The SR number
is a fact and joins nothing.

## Credit

The `attribution` line credits Fedlex.

## Turn it off

```toml
[sources.fedlex]
enabled = false
```

Fedlex and old `admin.ch/opc` links then get ordinary web cards.

## Examples

- `https://www.fedlex.admin.ch/eli/cc/1999/404/de` (the Federal Constitution)
- `https://www.fedlex.admin.ch/eli/cc/24/233_245_233/fr` (the Civil Code)
