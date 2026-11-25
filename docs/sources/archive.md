# `archive`: an archived copy of a gone page

This lookup family of [also on](../api.md#also-on) runs on a page that is gone or unreachable. It adds
the Wayback Machine capture closest to the time the page was first referenced.

| | |
| --- | --- |
| Source | `archive`, lookup family with the one source `wayback` |
| Runs on | `http` and `https` links whose status is `missing` or `unreachable` |
| Asks | The Wayback Machine's CDX API: `https://web.archive.org/cdx/search/cdx` |
| Credentials | None |
| Rate | At most 1 request every 2 seconds on `web.archive.org` |
| Rows live | 30 days; rows expire after twice that |
| Keys | None |
| Links | The Wayback Machine capture, `kind` `archived` |
| Facts | None |
| Switch | `[also_on.sources.wayback] enabled`, on by default |

## What it does

### `wayback`

1. The source asks the CDX API for up to five captures of the page's URL that answered 200, closest
   to the time the page was first referenced. When no reference time is known, it uses the time of
   the run.
2. It picks the closest capture itself, the earlier one on a tie.
3. The capture becomes a link, `https://web.archive.org/web/{timestamp}/{url}`.

When the page's status becomes anything else, its next also-on run removes the link.

Two kinds of page get no archived copy:

- a social post found gone: nothing is derived from it;
- a link that carries a credential: no other service is ever sent it.

archive.today is never asked.

## Keys, links and facts

The link is exact by URL and carries no key, so it joins nothing. Its `kind` is `archived`, its `via` is
`wayback` and its `at` is the capture's time.

A card has `site` `Wayback Machine` only on an archived copy's own page,
`web.archive.org/web/{timestamp}/{url}`.

## Cost

Each gone page costs one CDX request, cached for `limits.discovery_cache_secs` (an hour by default).
The pace covers the whole host, so the card of a capture that someone tagged waits behind the lookups
too. An error or a busy archive fails the run, which is retried ([runs](../operations.md#runs)).

## Turn it off

```toml
[also_on.sources.wayback]
enabled = false
```

After the restart, the source's stored links stop showing. They are kept until they expire and show
again when the source is on.
