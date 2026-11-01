# CourtListener

US court opinions from CourtListener, the Free Law Project's case-law archive, by opinion cluster
id, through its search API with the operator's token. Case law is in the public domain. The source
is off without a token: CourtListener's API answers no request without one (401), and its opinion
pages answer bots with a challenge, so opinion links then get ordinary web cards. The card carries
the source: "Source: CourtListener, Free Law Project".

## Token

`[sources.courtlistener] token`, or the environment variable `PINAKES_COURTLISTENER_TOKEN` (also
`PINAKES__SOURCES__COURTLISTENER__TOKEN`, which wins over the config file, which wins over the
short name): the API token of a CourtListener account. It is a secret: it is sent only to
`www.courtlistener.com`, as `Authorization: Token …`, never through a redirect, and never appears
in logs, errors, metrics, `Debug` output or cards. A token CourtListener refuses (401 or 403:
revoked or mistyped) turns the source off until the daemon restarts: opinion links get web cards
again, one warning is logged, and `pinakes_source_token_refusals_total{source="courtlistener"}`
counts each refusal.

Pinakes asks at most five requests a minute, the limit CourtListener set for anonymous use, so
cards arrive slowly when many opinions are new: a link whose request would wait more than a few
seconds for the host is retried when its turn comes, instead of timing out. A token's own limit may
be higher; `limits.host_rates` can raise the pace for `www.courtlistener.com`.

## Links

`https://www.courtlistener.com/opinion/{cluster}/` is the root; the opinion page with any slug
and its sub-pages (`/authorities/`, `/cited-by/`, `/summaries/`, …) and the API's
`/api/rest/v{3,4}/clusters/{cluster}/` are aliases of it. Docket pages (`/docket/…`) name a case,
which may have several opinions, and are not read.

## What it reads

`GET https://www.courtlistener.com/api/rest/v4/search/?type=o&q=cluster_id:{cluster}` (about
7 KB). A cluster the search does not find is `missing`.

## Card

`CreativeWork` (schema.org has no type for a court opinion), named by the case name and described
by the court, the filing date and the first citation.

| Fact | Meaning |
| --- | --- |
| `court` | E.g. Supreme Court of the United States |
| `date_filed` | `YYYY-MM-DD` |
| `citations` | E.g. `576 U.S. 644; 135 S. Ct. 2584; …` |
| `docket_number` | E.g. `14-556` |
| `case_name_full` | The full caption |
| `judge` | The opinion's author or judges |
| `precedential_status` | E.g. Published |
| `page_url` | The opinion's page on CourtListener |

## Examples

- `https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/`
