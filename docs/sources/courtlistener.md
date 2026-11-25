# CourtListener

Pinakes reads US court opinions from CourtListener, the Free Law Project's case-law archive, by
opinion cluster ID, through its search API. The source needs the operator's CourtListener API token
and is off without one: the API answers no request without a token (401), and the opinion pages
answer bots with a challenge, so opinion links then get ordinary web cards.

| | |
| --- | --- |
| Source | `courtlistener`, official source |
| Links | `courtlistener.com/opinion/{cluster}/…`, `courtlistener.com/api/rest/v3/clusters/{cluster}/` and `/v4/…` |
| Fetches | `GET https://www.courtlistener.com/api/rest/v4/search/?type=o&q=cluster_id:{cluster}` |
| Credentials | Required token: `[sources.courtlistener] token` or `PINAKES_COURTLISTENER_TOKEN`. Off without it |
| Rate | 5 requests a minute on `www.courtlistener.com` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `CreativeWork` |
| Keys | None (facts only) |
| Credit | "Source: CourtListener, Free Law Project", in `attribution` |
| Switch | `[sources.courtlistener] enabled`, on by default, but off while no token is set |

## Token

Set the API token of a CourtListener account in one of these places
([API keys](../api-keys.md)):

- `[sources.courtlistener] token` in the config file;
- `PINAKES__SOURCES__COURTLISTENER__TOKEN`;
- `PINAKES_COURTLISTENER_TOKEN`, which fills the token only when the other two leave it unset.

A token is at most 256 printable characters. It is a secret. Pinakes sends it only to
`www.courtlistener.com`, as `Authorization: Token …`, never through a redirect (a redirect is read
as the answer, not followed). It never appears in logs, errors or cards.

### A refused token

CourtListener refuses a token with a 401, or with a 403 whose JSON `detail` names the token (such as
"Invalid token.": revoked or mistyped). Pinakes then switches the token off for an hour:

- opinion links get web cards meanwhile, and nothing is sent to the API;
- one warning is logged;
- `pinakes_source_token_refusals_total{source="courtlistener"}` counts each refusal.

After the hour the token is sent again. Any other 403 (a forbidden object, a firewall's page) is a
failure of that link alone.

## Links

The root is `https://www.courtlistener.com/opinion/{cluster}/`. These links, on
`www.courtlistener.com` or `courtlistener.com`, are [aliases](../api.md#aliases) of it:

- the opinion page with any slug, `…/opinion/{cluster}/{slug}/`;
- its sub-pages, such as `…/authorities/`, `…/cited-by/` and `…/summaries/`;
- the API's `/api/rest/v3/clusters/{cluster}/` and `/api/rest/v4/clusters/{cluster}/`.

A cluster ID is one to twelve digits. Docket pages (`/docket/…`) name a case, which may have
several opinions, and are not read.

## What it fetches

One request per read:

```
GET https://www.courtlistener.com/api/rest/v4/search/?type=o&q=cluster_id:{cluster}
```

The card comes from the result whose `cluster_id` is the one asked for. A search with no such
result is `missing`. A rate limit (429), a timeout (408) or a server error is retried after at
least a minute. Any other status is an error.

At 5 requests a minute, cards arrive slowly when many opinions are new. A read whose request would
wait more than five seconds for the host's next slot is not sent: it is tried again when the slot
comes. A token's own limit may be higher; you can raise the pace for `www.courtlistener.com` with
`limits.host_rates`.

## Card

Every card is a `CreativeWork`; schema.org has no type for a court opinion.

- `name` is the case name.
- `description` joins the court, the filing date and the first citation, for example "Supreme
  Court of the United States · 2015-06-26 · 192 L. Ed. 2d 609".
- `attribution` is "Source: CourtListener, Free Law Project".

| Fact | Meaning |
| --- | --- |
| `court` | For example "Supreme Court of the United States" |
| `date_filed` | `YYYY-MM-DD` |
| `citations` | For example `576 U.S. 644; 135 S. Ct. 2584; …` |
| `docket_number` | For example `14-556` |
| `case_name_full` | The full caption |
| `judge` | The opinion's author or judges |
| `precedential_status` | For example "Published" |
| `page_url` | The opinion's page on CourtListener |

## Keys and groups

None. Citations and docket numbers are facts and join nothing.

## Credit

The `attribution` line credits CourtListener and the Free Law Project. Case law is in the public
domain.

## Turn it off

```toml
[sources.courtlistener]
enabled = false
```

Leaving the token unset has the same effect. Opinion links then get ordinary web cards, which may
show only CourtListener's challenge page.

## Examples

- `https://www.courtlistener.com/opinion/2812209/obergefell-v-hodges/`
