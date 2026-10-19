# Swiss Parliament

Business items of the Swiss Parliament (Curia Vista: motions, postulates, interpellations,
parliamentary initiatives, Federal Council bills…), from the Parliament's OData service by affair
id. Open data; no key; one request per second. The card carries the source: "Source: Swiss
Parliament, Parliamentary Services (parlament.ch)".

## Links

`https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId={id}` is the root.
The same page in French, Italian, English or Romansh, the legacy
`/{d|f|i}/suche/seiten/geschaefte.aspx?gesch_id={id}` page and the OData entity
`https://ws.parlament.ch/odata.svc/Business(ID={id},…)` are aliases of it. The affair id is the
year and a four-digit number: business 22.3765 is `20223765`.

## What it reads

`GET https://ws.parlament.ch/odata.svc/Business(ID={id},Language='DE')` with the card's fields
only (about 1 KB). The card is in German. An id the service does not know is `missing`.

## Card

`Legislation`, named by its title and described by its type and number, author, council and
status ("Po. 22.3765 · Glättli Balthasar · Nationalrat · Erledigt").

| Fact | Meaning |
| --- | --- |
| `business_number` | E.g. `22.3765` |
| `business_type` | E.g. Motion, Postulat, Interpellation |
| `status`, `status_date` | Its state and since when (`YYYY-MM-DD`) |
| `submitted_by`, `submission_date` | Who submitted it, and when |
| `council` | The council it was submitted in |
| `department` | The responsible federal department |
| `tags` | Its topics |
| `page_url` | Its Curia Vista page |

## Examples

- `https://www.parlament.ch/de/ratsbetrieb/suche-curia-vista/geschaeft?AffairId=20223765`
