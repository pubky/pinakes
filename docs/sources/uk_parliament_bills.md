# UK Parliament bills

Bills before the UK Parliament, and the Acts they became, from the Parliament's Bills API by bill
id. No key; one request per second. Open Parliament Licence v3.0: the card carries the attribution
"Contains Parliamentary information licensed under the Open Parliament Licence v3.0."

## Links

`https://bills.parliament.uk/bills/{id}` is the root; every page under it (`/stages`,
`/publications`, `/news`, …) and the API's `https://bills-api.parliament.uk/api/v1/Bills/{id}`
are aliases of it. The legacy `services.parliament.uk` bill pages carry no id and are not read.

## What it reads

`GET https://bills-api.parliament.uk/api/v1/Bills/{id}` (about 3 KB). An id the API does not know
is `missing`.

## Card

`Legislation`, named by its short title (the Act's name once it has Royal Assent) and described by
its long title.

| Fact | Meaning |
| --- | --- |
| `bill_type` | E.g. Government Bill, Private Members' Bill (Ballot Bill) |
| `current_stage` | E.g. Second reading, Royal Assent |
| `current_house`, `originating_house` | Commons, Lords or Unassigned |
| `is_act`, `defeated` | `true` when it became an Act, or was defeated |
| `withdrawn` | The date it was withdrawn |
| `former_title` | Its earlier short title |
| `sponsors` | The sponsoring members (or organisations) |
| `last_update` | `YYYY-MM-DD` |
| `page_url` | Its page on bills.parliament.uk |

## Examples

- `https://bills.parliament.uk/bills/3737` (the Employment Rights Act 2025)
