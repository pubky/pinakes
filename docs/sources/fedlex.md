# Fedlex

Swiss federal law in the classified compilation (the Constitution, federal acts, ordinances,
treaties), read through Fedlex's SPARQL endpoint (the JOLux model) by ELI. The Fedlex pages are
an application shell without metadata and are never read. No key; one request per second. The
card names its source: "Source: Fedlex, www.fedlex.admin.ch".

## Links

An act is named by its ELI, `eli/cc/{a}/{b}`, and its root is
`https://fedlex.data.admin.ch/eli/cc/{a}/{b}`. Every address of it on www.fedlex.admin.ch or
fedlex.data.admin.ch, in any language (`/de`, `/fr`, `/it`, `/rm`, `/en`) or article
(`#art_5`), is an alias of it.

A dated version, `…/eli/cc/{a}/{b}/{yyyymmdd}[/…]`, is its own resource with the root
`https://fedlex.data.admin.ch/eli/cc/{a}/{b}/{yyyymmdd}`: the act's card with a `version_date`
fact and the version's page. A date with a one-digit day (`2024033`) is an alias of the
eight-digit root.

An old classified-compilation address,
`https://www.admin.ch/opc/{lang}/classified-compilation/{number}/index.html`, is read by its
eight-digit number (the act's dossier) and becomes an alias of the act after its first fetch.
Official-compilation addresses (`eli/oc/…`) name a publication, not the act in force, and are not
read.

## What it reads

`GET https://fedlex.data.admin.ch/sparqlendpoint?query=…` for the act's title, abbreviation and
language versions, SR number, dates, status and type (about 6 KB); for an old address first the
act with that dossier number. An ELI with no act is `missing`.

## Card

`Legislation`, named by its German title (else French, Italian, English, Romansh) and described by
its SR number, abbreviation and status ("SR 101 · BV · In force").

| Fact | Meaning |
| --- | --- |
| `sr_number` | Its number in the classified compilation, e.g. `101`, `210` |
| `abbreviation` | E.g. `BV`, `ZGB` |
| `english_title` | Its English title, where Fedlex has one |
| `document_date`, `entry_into_force` | `YYYY-MM-DD` |
| `applicable_version` | The date of the version in force today |
| `version_date` | For a dated version: its date (`YYYY-MM-DD`) |
| `status` | E.g. In force, No longer in force |
| `document_type` | E.g. Federal act |
| `page_url` | Its page on Fedlex (German) |

## Examples

- `https://www.fedlex.admin.ch/eli/cc/1999/404/de` (the Federal Constitution)
- `https://www.fedlex.admin.ch/eli/cc/24/233_245_233/fr` (the Civil Code)
