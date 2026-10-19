# Operations

## Configuration

`pinakes.example.toml` lists the operator choices: addresses and credentials, each resolver's
and feature's `enabled` switch, egress allow and deny lists, and the public API's budget. Every cap, timeout, budget,
backoff and refresh tier is a limit: it has a default and can be set in an optional `[limits]`
section (or as `PINAKES__LIMITS__<NAME>`). The web limits cover the web resolver; the resolver
limits apply to pubky and to each card resolver.

| Limit | Default | Meaning |
| --- | --- | --- |
| `poll_secs` | `5` | Nexus feed poll interval in seconds. |
| `grace_days` | `7` | Days an unreferenced resource is kept before orphan cleanup. |
| `refresh_resource_secs` | `[604800, 172800, 21600, 3600]` | Resource refresh intervals by tier (seconds). |
| `refresh_container_secs` | `[86400, 43200, 21600, 3600]` | Container (calendar, feed) refresh intervals by tier (seconds). |
| `refresh_cursor_secs` | `300` | Cursor feed refresh interval (seconds). |
| `change_retention_days` | `90` | Days change lines are retained. |
| `canary_interval_secs` | `21600` | Adapter canary interval (seconds). |
| `web_concurrency` | `4` | Concurrent jobs of the web resolver. |
| `web_requests_per_second` | `4.0` | Jobs per second of the web resolver. |
| `resolver_concurrency` | `2` | Concurrent jobs per pubky or card resolver. |
| `resolver_requests_per_second` | `2.0` | Jobs per second per pubky or card resolver. |
| `resolver_ttl_secs` | `86400` | Default document lifetime (seconds) for web, pubky and card resolvers. |
| `host_requests_per_second` | `1.0` | Requests per second to any one upstream host. |
| `host_rates` | `{}` | Per-host overrides of `host_requests_per_second`. |
| `max_failures` | `5` | Failures before a resource dead-letters. |
| `timeout_grace` | `3` | Timeouts tolerated before they count as failures. |
| `backoff_secs` | `30` | First retry backoff (seconds). |
| `max_backoff_secs` | `3600` | Retry backoff ceiling (seconds). |
| `lease_secs` | `60` | Job lease (seconds). |
| `missing_ttl_secs` | `3600` | Re-check interval for missing resources (seconds). |
| `author_requests_per_second` | `1.0` | Resolution budget per author per second. |
| `max_label_bytes` | `128` | Longest accepted tag label (bytes). |
| `max_object_path_bytes` | `2048` | Longest accepted Pubky object path (bytes). |
| `osm_ttl_secs` | `2592000` | OSM place cache lifetime (seconds). |
| `osm_dump_every_hours` | `24` | Hours between two writes of the ODbL dump file. |
| `osm_dump_downloads_per_hour` | `6` | ODbL dump downloads per hour per client address. |
| `api_cache_capacity` | `10000` | API response cache entries. |
| `preview_requests_per_minute` | `30` | Preview requests per minute per client. |
| `preview_uri_requests_per_minute` | `30` | Preview requests per minute per URI. |
| `preview_cache_capacity` | `10000` | Preview cache entries. |
| `podcast_items` | `1000` | Podcast feed items read per scan. |
| `group_max_members` | `256` | Most members of one automatic group. |
| `group_max_distance_m` | `500.0` | Farthest two located members of one group (metres). |
| `group_max_graph_units` | `20000` | Units one grouping pass may read. |
| `group_max_graph_bytes` | `33554432` | Bytes one grouping pass may read. |


## Database

Supported: PostgreSQL 16–18, PostGIS 3.4+, `pg_trgm` and h3.

`pinakesd --config <path> --migrate` applies the migrations in `migrations/` and exits; the
daemon applies them at startup too. Pinakes rebuilds everything from the Nexus feed, so a
database can also be recreated:

```sh
dropdb pinakes && createdb pinakes
pinakesd --config pinakes.toml --migrate
```

A database advisory lock allows one daemon per database. Resolver, host, author and IP
budgets are process-local, and the lock keeps their guarantees.

**Re-extraction.** A parser or routing change reaches stored documents through one command, run
while the daemon is stopped (it takes the instance lock):

```sh
pinakesd --config <path> reextract <route…|all>
```

It re-runs the current classification on the selected rows (all rows, or those routed to or
classified as one of the named routes or card kinds) and re-queues them at once, in batches of
1000. Each loses its HTTP validators and, on the OSM route, its `osm_place` cache entry, so the
next fetch re-extracts. A row entering or leaving the OSM route swaps its identity envelope and
becomes pending, as does an OSM-host URL moving to web. Leases are fenced by a generation bump.
Aliases have no route and are skipped; a row whose URI now has a static alias root (a new
tracking parameter, say) becomes an alias of it.

**Dead letters.** To retry one by hand:

```sql
UPDATE resource SET dead_letter=false, attempts=0, next_attempt_at=now(),
  lease_until=NULL, generation=generation+1 WHERE id='<resource_id>';
```

**Grouping rebuild.** The daemon stores its grouping policy (its limits and mode) and
re-queues every membership when it changes, so a changed `groups.cross_container_occurrences`
takes effect as that queue drains. To rebuild by hand:

```sql
DELETE FROM group_input;
INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;
```

## Deployment and egress

Web fetches bypass environment proxies, validate every DNS answer, pin the checked addresses and
repeat the policy at every redirect (three hops; HTML meta refresh and literal JavaScript
redirects count). `egress.allow_hosts` and `egress.deny_hosts` take exact names or
`*.example.org`; they never override the IP policy, and deny wins. Pages are capped at 1 MB of
decoded body; an oversized page keeps its head metadata with a `truncated` warning. Requests
time out after ten seconds. Images stay origin URLs and are never fetched.

Two clients are outside that policy. Nominatim and Nexus are trusted, operator-configured
upstreams at fixed URLs that no reference can change: their clients do not apply the web DNS
pinning or the private-address refusal (Nexus defaults to loopback), so configure their URLs and
DNS through trusted controls. Their Retry-After delays are capped at one hour. The public
Nominatim is for a small development run only: its [usage
policy](https://operations.osmfoundation.org/policies/nominatim/) requires an identifying
User-Agent, caching and at most one request per second, and `osm.requests_per_second` defaults
to 0.5 and is refused above 1 for a public host. Staging and production use their own
`osm.nominatim_url`. Pinakes never proxies place search, autocomplete or reverse geocoding.

The ODbL share-alike download is a file: the daemon writes `osm_place.ndjson.gz` into
`osm.dump_dir` (default `dumps`, relative to the working directory) at startup when it is missing
or older than `limits.osm_dump_every_hours` (24), then at that interval. One writer per
directory: the writer holds `osm_place.ndjson.gz.lock` (a file lock), so daemons sharing the
directory take turns and one skips a file another has just written; holding it, the writer, and
the daemon at startup, remove the temporary files an interrupted write left. It reads one repeatable-read snapshot
without a statement timeout, compresses it into a temporary file of its own
(`osm_place.ndjson.gz.{pid}.{random}.tmp`) off the async workers, syncs it and renames it into
place; a failed write, or one refused because another daemon holds the lock, keeps the previous
file and is retried after five minutes. Shutdown does not wait for a write in progress: it is
cancelled, the previous file stays, and the next start removes its temporary file. The directory
must be writable by the daemon and hold two copies of the file while one is written: about 1.5 KB per cached place
uncompressed, several times less gzipped. Downloads read the file only (no database work),
`limits.osm_dump_downloads_per_hour` (6) transfers per address (`HEAD`, 304 and 416 are free);
a CDN or reverse proxy may cache it by its `ETag`.

### Pubky

Pinakes fetches a `pubky://` object through the egress policy above. It reads the owner's
`_pubky` PKARR record (following a delegation to the homeserver's key, at most seven deep) from
the PKARR relays in `pubky.pkarr_relays` (by default `https://pkarr.pubky.app` and
`https://pkarr.pubky.org`), asked together: a GET through the egress policy (an answer above
8 KB refused, whatever its content type), a 404 meaning no record. Pinakes checks every packet's
signature against the owner's key and uses the newest packet that verifies. A relay can withhold
a record (when every relay answers 404, the owner has no record for five minutes) but cannot
forge one, and Pinakes never uses a packet older than the newest it remembers for that key. That
memory is an accepted limit: it holds the newest timestamp of at most 10,000 keys, apart from the
cache, and forgets the oldest timestamps of uncached keys first; a forgotten key, or any key
after a restart, takes the newest packet the relays serve again. A relay that times out, is
refused by the policy or serves a packet that does not verify is a `connect` failure, retried
with the usual backoff and, like any failure, dead-lettered after `limits.max_failures`.
Verified packets are kept for their smallest record TTL, between five minutes
and a day (10,000 keys at most). It then takes the first endpoint with a public HTTPS (ICANN)
domain and GETs the object path there with the `pubky-host` header, as Pubky clients address
ICANN endpoints. The usual rules apply: every DNS answer checked and pinned, private, loopback,
link-local and metadata addresses refused, HTTPS only, no proxy, the ten-second deadline, the 1
MB cap and the content-type allowlist, which for Pubky objects also admits
`application/octet-stream`, `text/plain` and unlabelled bodies (homeservers label objects by
magic bytes or extension, so JSON arrives as `application/octet-stream`). The object is read as
JSON whatever its declared type; one that is not JSON becomes a card with only its content type
and size. Because the request carries a header, a redirect is refused rather than followed.
Refusals by the policy (a redirect, a content type outside the list, a non-public address, an
owner host that is not the canonical z32 key, a path that changes when read as an HTTPS path, an
endpoint that is an IP address rather than a domain) are stored as `policy` and dead-lettered at
the first attempt instead of retried. **A homeserver that publishes only key-pinned
(raw-public-key TLS) endpoints is unreachable** (`connect`, with a message saying so).

Homeservers count against the per-host budget (`limits.host_requests_per_second`, one request a
second by default) and the egress allow and deny lists like any host. Most objects live on a few
homeservers, so a busy one may need its own rate in `limits.host_rates`
(`"homeserver.example" = 4.0`). The Pubky resolver's own limits
(`limits.resolver_concurrency`, `limits.resolver_requests_per_second`: 2 and 2.0) and the author
budget apply on top. The relays count against the per-host budget too; the record cache keeps
their load to one request per owner and TTL.

### Clients and budgets

Every public `/v1` route shares the direct-peer IP budget (`api.requests_per_minute`), and
previews have their own per-IP and per-URI budgets. Forwarded headers are deliberately ignored:
behind a reverse proxy the proxy's address gets the aggregate budget, so set client-IP limits at
the edge.

The admin listener (`admin_listen`, default `127.0.0.1:9090`) requires `admin_token` (or
`PINAKES__ADMIN_TOKEN`) for a non-loopback bind; terminate TLS at a trusted reverse proxy.

## Ingestion

`pinakesd` polls the Nexus reference feed every `limits.poll_secs`. Pinakes keys each line on its
`uri`, normalized and hashed into its own ID (Nexus's `resource_id` is not read); a URI that does
not normalize is rejected (`uri`). A line's author must be 52
lowercase z-base-32 characters; labels and object paths must be non-empty, NUL-free and within
`limits.max_label_bytes` and `limits.max_object_path_bytes`. A bad line is skipped and counted in
`pinakes_ingest_rejected_total{reason}` (`author`, `label`, `object_path`, `uri`, `timestamp`,
`event`, `panic`) after the batch commits; the cursor still advances. Each line runs in a
savepoint: a line whose processing panics is rolled back alone and skipped (`panic`; the log
line names its feed position only, never its content). A panic anywhere else in a poll (the
batch's score update or cursor write, say) is an error: the batch rolls back, the cursor holds,
`pinakes_ingest_panics_total` counts it and the poll is retried. A feed epoch change, or a
cursor beyond the feed's length, clears the reference projection and scores and restarts at
zero; resource documents stay through the grace period. A resource without references is
deleted `limits.grace_days` after it became orphaned, previews included. A Nexus outage leaves
cached data served and polling retries.

## Queue and refresh

The durable queue leases rows with `SKIP LOCKED` and fences stale completions by a generation
counter. A job lease lasts at least `limits.lease_secs`; a 50-object OSM batch gets 760 seconds.
Failures back off exponentially from `limits.backoff_secs` to `limits.max_backoff_secs` and
dead-letter after `limits.max_failures`. The first `limits.timeout_grace` consecutive timeouts
back off without charging an attempt. A missing result needs a second fresh fetch to confirm,
scheduled after `limits.missing_ttl_secs`; a cached copy cannot count twice. A lookup or preview
moves an overdue resource to the front within its TTL, author budget and retry schedule; it
cannot revive a dead letter. Resolution is also limited per referencing author
(`limits.author_requests_per_second`); several tags never multiply fetches.

Refresh follows the time to an event's start (`limits.refresh_resource_secs`,
`limits.refresh_container_secs`): 7 days, 2 days, 6 hours and 1 hour. One fetch at end + 24
hours makes a resource final, and a new reference reopens a final resource; `geo:` stays final.
OSM places refresh after `limits.osm_ttl_secs`; a confirmed missing object stays cached as
missing for the same period. Conditional responses keep accepted facts.

## Web pages and access walls

Generic web resolution reads JSON-LD, microformats, OpenGraph, oEmbed and the title. Canonical,
alternate and oEmbed links come from the document head only. Media pages use the pinned
390-provider oEmbed registry (`data/`): `media.oembed = "when_needed"` (default) asks
path-specific providers first and broad patterns only for an incomplete, failed or truncated
page; `"always"` also fetches every matching provider and trusted page-advertised endpoints,
which must share the page's origin or its registry provider. Healthy page JSON-LD stays
authoritative and oEmbed fills gaps. `web.discover_feeds` (web pages) and
`cards.media.discover_feeds` (media pages: podcast feed scanning) follow RSS alternates on pages
without a typed card. Discovery decisions are counted in
`pinakes_discovery_decisions_total{kind,decision}`.

A challenge page (a bot wall, captcha or sign-in redirect) is a failed fetch: the resource turns
`unreachable` and is retried with the normal backoff, dead-lettering after
`limits.max_failures`. Its last accepted document stays, and a wall never counts toward a
disappearance. Fetches that hit one count as outcome `blocked` in
`pinakes_fetch_total`.

The markers are the built-in `data/interstitial-markers.json` (text markers, captcha selectors,
wall host prefixes, sign-in paths). Visible text excludes `script`, `style`, `noscript` and
`template`. Title, heading and captcha markers count only on short pages (under 2,000 visible
bytes) or challenge statuses (403, 429, 503). A redirect of the requested path to a listed
sign-in path (`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall unless the
requested path is itself one. A meta refresh without a URL is content; a refresh slower than ten
seconds is a hop only on a short page that does not refresh itself.

## Groups

Grouping runs after a document commits, from its own dirty queue with its own backoff (5–10
seconds of jitter, growing to an hour), so a grouping failure never blocks a save. A document
records its evidence as group keys: an identifier card's `doi`/`isbn` and the landing pages its
registry names (`registry`), a page's head citations (`cites_doi`, `cites_isbn`) and ICS UIDs
(`ics_uid`). Groups form only from those: an identifier card with
a named landing page that cites it, one UID within one feed, and curation. A group stores
membership only; cards read it when they are served. Each key of a run may
make a capped number of compatibility checks (`limits.group_max_members`² × 2, between 100,000
and 2,000,000). A key that alone exceeds the cap is parked (its unions undone) and the later
keys still run; the run settles and counts it
(`pinakes_group_budget_resources{state="parked"}`). A run stops at four caps and retries within
the hour with `last_error = 'work_budget'` (`state="stopped"`), starting at the key it stopped
at, so a component too large for one run is covered in turn; after 24 stopped runs on unchanged
input it settles with the groups the last run left until its input changes (`state="settled"`).
Parked keys' checks count against the run. Past either limit, every set of units holding one the
run did not finish keeps each member's group, and no other set takes that group's ID; in a
stopped run so does every set that `limits.group_max_members` kept from joining another, since a
resumed run meets the keys in another order. A member deleted by orphan expiry leaves its group
at once. Writer lock contention retries after one to three seconds; curation waits up to two
seconds and answers 503 with Retry-After. `groups.cross_container_occurrences = "corroborated"`
also joins one UID across feeds when the instances are within 500 m, 300 s and a name similarity
of 0.5; a determined publisher can copy that, so it is off by default.

Tenant rules (`data/multi-tenant-hosts.json`) say which part of a multi-tenant host is one
publisher; the canonical alias rule needs the same tenant. A rule is
`"subdomain"`, `"first_path_segment"`, `{"path_segment":N}` or
`{"path":{"segment":N,"skip":["u/*"],"extend":["s"],"query":["src"]}}`: `skip` drops a noise
window (Google Calendar's `u/{n}`), `extend` takes the next segment too (anchor.fm `/s/{id}`),
and `query` scopes by a parameter when the path is short (`embed?src=`).

## Monitoring

`GET /metrics` on the admin listener serves Prometheus series per source: fetch outcomes,
changes, canaries, the queue, ingestion lag and grouping. Admin `GET /v1/info` has the
manifests, resolver status, counts, ingestion, source and canary state; public `/v1/info` shows
only the version, what the instance resolves, the limits a client must respect and the OSM
licence. Stored errors are sanitized classes (dns, connect, timeout, http_4xx, http_5xx, parse,
policy). Admin info and database metrics are cached for 15 seconds.

Canaries (`canary.enabled`) run every `limits.canary_interval_secs`, first one interval after
startup, on the ordinary workers and host budgets; their state survives restarts and they never
write documents, the OSM cache or changes. A successful result missing a declared field is
`outcome=drift`; generic web declares no fields. A canary denied by host policy is skipped, and
an over-budget sample is inconclusive. See [ops/README.md](../ops/README.md) for alert rules, the
dashboard and scrape setup; `scripts/check-ops` checks every alert and dashboard metric against
the registry (and runs `promtool check rules` when installed).

The capability-URL guard (`scripts/verify-no-capability-urls`, run by the tests) scans every
tracked and new source file for capability URLs; synthetic fixture URLs are allowed only in
named test files, and failures print paths without the URLs.
