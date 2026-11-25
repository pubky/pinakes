# Zefix

Pinakes reads Swiss companies in the commercial register as linked data from LINDAS, the
Confederation's linked-data service, by the company's register ID (EHRA ID). No key is needed.
Pinakes never fetches the zefix.ch pages.

| | |
| --- | --- |
| Source | `zefix`, official source |
| Links | `register.ld.admin.ch/zefix/company/{id}[/…]`; `zefix.ch/{lang}/search/entity/list/firm/{id}` |
| Fetches | `GET https://ld.admin.ch/query?query=…` (one SPARQL query) |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Organization` |
| Keys | None (facts only) |
| Credit | "Source: Zefix, Federal Office of Justice (EHRA), via LINDAS", in `attribution` |
| Switch | `[sources.zefix] enabled`, on by default |

## Links

A company is named by its EHRA ID, a positive number of up to nine digits. The root is
`https://register.ld.admin.ch/zefix/company/{id}`. These links are [aliases](../api.md#aliases) of
it:

- its sub-resources on `register.ld.admin.ch`, such as `…/address` or `…/UID/{uid}`;
- its zefix.ch page, `https://www.zefix.ch/{lang}/search/entity/list/firm/{id}`, in German,
  French, Italian or English (`de`, `fr`, `it`, `en`), with any query.

Not read here: searches on zefix.ch (`…/search/entity/list?mainSearch=…`) and a UID (`CHE-…`)
alone.

## What it fetches

One SPARQL query per read, as a GET to `https://ld.admin.ch/query?query=…`. It asks for every
statement about the company, its address and its identifiers, and the names of its legal form and
seat.

A company without a legal name in the answer is `missing`. A 429 is retried after at least the time
LINDAS names (up to an hour), or 5 minutes when it names none. A timeout (408) or a server error is
retried after at least a minute. Any other status is an error.

## Card

Every card is an `Organization`.

- `name` is the company's legal name.
- `description` is its purpose, as registered.
- `address` holds its registered street, postcode and city, and the country code `ch`.
- `attribution` is "Source: Zefix, Federal Office of Justice (EHRA), via LINDAS".

| Fact | Meaning |
| --- | --- |
| `uid` | Its business identification number, for example `CHE-105.909.036` |
| `ch_id` | Its CH-ID, for example `CH17030010013` |
| `ehra_id` | Its EHRA ID, as in the link |
| `legal_form` | Its legal form, in German where LINDAS has it, for example "Aktiengesellschaft" |
| `seat` | The municipality of its registered seat, for example "Cham" |
| `canton` | The canton of its registered address, for example `ZG` |
| `other_names` | Its names, each with its language tag, for example `Nestlé S.A. (fr)` |
| `page_url` | Its page on zefix.ch, in English |

## Keys and groups

None. The UID, CH-ID and EHRA ID are facts and join nothing.

## Credit

The `attribution` line credits Zefix and the Federal Office of Justice's commercial register office
(EHRA), via LINDAS.

## Turn it off

```toml
[sources.zefix]
enabled = false
```

LINDAS company links and zefix.ch company pages then get ordinary web cards.

## Examples

- `https://register.ld.admin.ch/zefix/company/126286`
