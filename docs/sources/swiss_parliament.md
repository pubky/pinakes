# Swiss Parliament

Pinakes reads business items of the Swiss Parliament (Curia Vista: motions, postulates,
interpellations, parliamentary initiatives, Federal Council bills and more) from the Parliament's
OData service, by affair ID. No key is needed. Cards are in German.

| | |
| --- | --- |
| Source | `swiss_parliament`, official source |
| Links | `parlament.ch/{lang}/ratsbetrieb/suche-curia-vista/geschaeft?AffairId={id}`; legacy `parlament.ch/{d,f,i}/suche/seiten/geschaefte.aspx?gesch_id={id}`; `ws.parlament.ch/odata.svc/Business(ID={id},…)` |
| Fetches | `GET https://ws.parlament.ch/odata.svc/Business(ID={id},Language='DE')?$select=…&$format=json` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Legislation`, `CreativeWork` |
| Keys | None (facts only) |
| Credit | "Source: Swiss Parliament, Parliamentary Services (parlament.ch)", in `attribution` |
| Switch | `[sources.swiss_parliament] enabled`, on by default |

## Links

An item is named by its affair ID: the four-digit year and a four-digit number, so business 22.3765
is `20223765`. The ID is eight digits and starts with `19` or `20`.

The root is the German Curia Vista page,
`https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId={id}`. These links,
on `www.parlament.ch` or `parlament.ch`, are [aliases](../api.md#aliases) of it:

- the same page in French, Italian, English or Romansh (`/fr/`, `/it/`, `/en/`, `/rm/`), with the
  `AffairId` parameter in any letter case;
- the legacy page, `/{d|f|i}/suche/seiten/geschaefte.aspx?gesch_id={id}` (or `/pages/`);
- the OData entity, `https://ws.parlament.ch/odata.svc/Business(ID={id},…)`, in any language.

Not read here: the Curia Vista search page, other language codes in the path, and an affair ID in
the short form (`223765`).

## What it fetches

One request per read, for the German item with only the fields the card needs:

```
GET https://ws.parlament.ch/odata.svc/Business(ID={id},Language='DE')?$select=…&$format=json
```

An ID the service doesn't know (404) is `missing`. An answer for another item, or one without a
title, is an error. A rate limit (429), a timeout (408) or a server error is retried after at least
a minute. Any other status is an error.

## Card

The type follows the business type:

- `Legislation` for a Federal Council bill ("Geschäft des Bundesrates"), a Parliament bill
  ("Geschäft des Parlaments"), a cantonal initiative ("Standesinitiative") and a parliamentary
  initiative ("Parlamentarische Initiative");
- `CreativeWork` for every other type, such as motions, postulates, interpellations and questions.

- `name` is the item's title.
- `description` joins the type's abbreviation and the business number, the submitter, the council
  and the status, for example "Po. 22.3765 · Glättli Balthasar · Nationalrat · Erledigt".
- `attribution` is "Source: Swiss Parliament, Parliamentary Services (parlament.ch)".

| Fact | Meaning |
| --- | --- |
| `business_number` | For example `22.3765` |
| `business_type` | In German, for example "Postulat" |
| `status` | Its state, in German, for example "Erledigt" |
| `status_date` | Since when it has that state, `YYYY-MM-DD` (UTC) |
| `submitted_by` | Who submitted it |
| `submission_date` | When it was submitted, `YYYY-MM-DD` (UTC) |
| `council` | The council it was submitted in, for example "Nationalrat" |
| `department` | The responsible federal department, by abbreviation, for example `WBF` |
| `tags` | Its topics, in German |
| `page_url` | Its Curia Vista page (its root) |

## Keys and groups

None.

## Credit

The `attribution` line credits the Swiss Parliament's Parliamentary Services. The Parliament
publishes this data as open data.

## Turn it off

```toml
[sources.swiss_parliament]
enabled = false
```

parlament.ch business pages and OData links then get ordinary web cards.

## Examples

- `https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765`
