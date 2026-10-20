# CourtListener

US court opinions from CourtListener, the Free Law Project's case-law archive, by opinion cluster
id, through its search API. Case law is in the public domain; no key. Without a token
CourtListener answers at most five requests per minute, which is this host's budget, so cards
arrive slowly when many opinions are new: a link whose request would wait more than a few seconds
for the host is retried when its turn comes, instead of timing out. The card carries the source: "Source: CourtListener,
Free Law Project".

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
