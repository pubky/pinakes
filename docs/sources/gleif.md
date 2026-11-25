# GLEIF

Pinakes reads Legal Entity Identifier (LEI) records from the API of the Global Legal Entity
Identifier Foundation (GLEIF), by LEI. No key is needed.

| | |
| --- | --- |
| Source | `gleif`, official source |
| Links | `lei:{LEI}`; `gleif.org/…/lei/{LEI}`; `api.gleif.org/api/v1/lei-records/{LEI}` |
| Fetches | `GET https://api.gleif.org/api/v1/lei-records/{LEI}` |
| Credentials | None |
| Rate | 1 request per second on `api.gleif.org` (GLEIF allows 60 a minute) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Organization` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.gleif] enabled`, on by default |

## Links

The root is `lei:{LEI}`, with the LEI in upper case. These links are [aliases](../api.md#aliases)
of it:

- `lei:{lei}` in lower case;
- `https://www.gleif.org/…/lei/{LEI}` (also on `gleif.org`), for any path that ends in `/lei/{LEI}`;
- `https://api.gleif.org/api/v1/lei-records/{LEI}`.

An LEI is 20 characters (ISO 17442): 18 letters and digits, then two check digits. One that fails
the check is never read, and a `lei:` root with an invalid LEI is `missing` at once and never
fetched.

Not read here:

- the record's sub-resources (`…/direct-parent`, `…/ultimate-parent`, `…/direct-children`,
  `…/isins`), which name other entities or other data;
- GLEIF's search page, `https://search.gleif.org/#/record/{LEI}`. It names the record only in the
  URL fragment, which a resource's identity drops, so these links all name the same page.

## What it fetches

One request per read: `GET https://api.gleif.org/api/v1/lei-records/{LEI}`. An LEI GLEIF doesn't
know (404) is `missing`. A record for another LEI, or one without a legal name, is an error, not a
card. A rate limit (429), a timeout (408) or a server error is retried after at least a minute. Any
other status is an error.

## Card

Every card is an `Organization`.

- `site` is "GLEIF", `name` the legal name.
- `description` joins the LEI, the legal address's city and country, and the entity status in lower
  case, for example "LEI KY37LUS27QQX7BB93L28 · VEVEY, CH · active".
- `address` holds the legal address: its street lines, postcode, city and country code (in lower
  case).
- `page_url` is the record on GLEIF's search site. A `lei:` card opens that page
  ([`open_in`](../api.md)).

| Fact | Meaning |
| --- | --- |
| `lei` | The LEI |
| `jurisdiction` | For example `CH`, `US-DE` |
| `legal_form` | The ISO 20275 entity legal form code (for example `MVII`), else GLEIF's free text |
| `entity_status` | For example `ACTIVE` |
| `registration_status` | The LEI's registration status, for example `ISSUED` |
| `registered_as` | The entity's number at its business register, for example `CHE-105.909.036` |
| `registered_at` | That business register's GLEIF code |
| `bic` | Its BIC codes |
| `opencorporates` | Its OpenCorporates ID |
| `last_update` | When the record last changed, `YYYY-MM-DD` |
| `page_url` | `https://search.gleif.org/#/record/{LEI}` |

## Keys and groups

None. The LEI, BIC codes and register numbers are facts and join nothing. An SEC company card can
carry the same LEI as its `lei` fact, but the two cards don't join.

## Credit

The card credits nothing. GLEIF publishes LEI data under CC0.

## Turn it off

```toml
[sources.gleif]
enabled = false
```

gleif.org and api.gleif.org links then get ordinary web cards, and `lei:` roots wait until the
source is on again.

## Examples

- `lei:KY37LUS27QQX7BB93L28`
- `https://api.gleif.org/api/v1/lei-records/5493001KJTIIGC8Y1R12`
