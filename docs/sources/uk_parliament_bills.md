# UK Parliament bills

Pinakes reads bills before the UK Parliament, and the Acts they became, from the Parliament's Bills
API, by bill ID. No key is needed.

| | |
| --- | --- |
| Source | `uk_parliament_bills`, official source |
| Links | `bills.parliament.uk/bills/{id}` and its tabs; `bills-api.parliament.uk/api/v1/Bills/{id}` |
| Fetches | `GET https://bills-api.parliament.uk/api/v1/Bills/{id}` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation` |
| Keys | None (facts only) |
| Credit | "Contains Parliamentary information licensed under the Open Parliament Licence v3.0.", in `attribution` |
| Switch | `[sources.uk_parliament_bills] enabled`, on by default |

## Links

The root is `https://bills.parliament.uk/bills/{id}`. These links are
[aliases](../api.md#aliases) of it:

- one tab of the bill's page, `…/bills/{id}/{tab}`, such as `/stages`, `/publications` or `/news`;
- the API's `https://bills-api.parliament.uk/api/v1/Bills/{id}`.

A bill ID is a positive number of up to seven digits.

Not read here:

- deeper pages, such as one stage's amendments (`…/stages/{stage}/amendments`) or one publication;
  they name other objects;
- the API's sub-resources, such as `…/Bills/{id}/Stages`;
- the bills list, `https://bills.parliament.uk/bills`;
- the legacy `services.parliament.uk` bill pages, which carry no bill ID.

## What it fetches

One request per read: `GET https://bills-api.parliament.uk/api/v1/Bills/{id}`. An ID the API
doesn't know (404) is `missing`. An answer for another bill, or one without a short title, is an
error. A 429 is retried after at least the time the API names (up to an hour), or 5 minutes when it
names none. A timeout (408) or a server error is retried after at least a minute. Any other status
is an error.

## Card

Every card is `Legislation`.

- `name` is the short title the API gives, for example "Employment Rights Act 2025" for bill 3737.
- `description` is the long title.
- `attribution` is "Contains Parliamentary information licensed under the Open Parliament Licence
  v3.0."

| Fact | Meaning |
| --- | --- |
| `bill_type` | The bill type, by the API's type ID: for example "Government Bill", "Private Members' Bill (Ballot Bill)". Absent for a type ID Pinakes doesn't know |
| `current_stage` | The stage it has reached, for example "Royal Assent" |
| `current_house` | The house the bill is in, as the API names it, for example `Commons` or `Unassigned` |
| `originating_house` | The house the bill started in, for example `Commons` |
| `is_act` | `true` when the bill became an Act; absent otherwise |
| `defeated` | `true` when the bill was defeated; absent otherwise |
| `withdrawn` | The date it was withdrawn, `YYYY-MM-DD` |
| `former_title` | Its earlier short title |
| `sponsors` | The sponsoring members, or organisations |
| `last_update` | `YYYY-MM-DD` |
| `page_url` | Its page on bills.parliament.uk (its root) |

## Keys and groups

None. A bill and the Act it became on legislation.gov.uk are separate resources and don't join.

## Credit

The `attribution` line credits the data under the Open Parliament Licence v3.0.

## Turn it off

```toml
[sources.uk_parliament_bills]
enabled = false
```

bills.parliament.uk and Bills API links then get ordinary web cards.

## Examples

- `https://bills.parliament.uk/bills/3737` (the Employment Rights Act 2025)
