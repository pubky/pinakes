# Sources: cards from official APIs

Some services answer exactly from their own API where their page is walled, thin or has moved.
For their links, Pinakes reads that API instead of the page. Every source here:

- reads one object by an identifier taken from the link (never a search by title or name);
- needs no key;
- makes every address of the same object one resource: the other addresses are
  [aliases](../api.md#aliases) of one root;
- records itself on the card: `details.from` names the source and `details.facts` holds the
  fields its API names for the object, by name (lists joined with `; `).

## Switches

Each source has its own switch, on by default:

```toml
[sources.federal_register]
enabled = false
```

(or `PINAKES__SOURCES__FEDERAL_REGISTER__ENABLED=false`). A switched-off source's web links get
ordinary web cards; a link in its own identifier scheme (such as `fr:…`) waits until the source is
on again. The whole route can be switched off with `[cards.official] enabled = false`.

## Politeness

Every request carries the operator's contact in the User-Agent (`http.contact`, see
[operations](../operations.md#deployment-and-egress)); a source whose service asks for an email
address is skipped while it is empty. Requests share the per-host budget
(`limits.host_requests_per_second`, one per second by default) and never have more than one
request in flight per host; a source whose service publishes a limit sets that host's rate (a
windowed limit as its average rate), and `limits.host_rates` overrides it. Each page below names
the service's limits, licence and any attribution the card carries.

After an upgrade that adds a source, run `pinakesd reextract` for its hosts, so links that failed
or got plain web cards before are read again.

## Sources

- [`ecfr`](ecfr.md): sections and parts of the Code of Federal Regulations.
- [`federal_register`](federal_register.md): Federal Register documents by document number.
- [`gleif`](gleif.md): Legal Entity Identifier records.
- [`sec`](sec.md): SEC EDGAR filings, their documents and companies.
- [`swiss_parliament`](swiss_parliament.md): Swiss Parliament business items (Curia Vista) by affair id.
- [`uk_parliament_bills`](uk_parliament_bills.md): UK Parliament bills and Acts by bill id.
