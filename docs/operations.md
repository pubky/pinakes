# Operations

## Configuration

`pinakes.example.toml` lists the operator choices: addresses and credentials, each resolver's
and feature's `enabled` switch, the event platforms (`enabled`, `key`), egress allow and deny
lists, and the public API's budget, CORS origins and Swagger UI. Every cap, timeout, budget,
backoff and refresh tier is a limit: it has a default and can be set in an optional `[limits]`
section (or as `PINAKES__LIMITS__<NAME>`). The web limits cover the web resolver and the event
routes on it; the resolver limits apply to pubky and to each card resolver.

| Limit | Default | Meaning |
| --- | --- | --- |
| `poll_secs` | `5` | Nexus feed poll interval in seconds. |
| `grace_days` | `7` | Days an unreferenced resource is kept before orphan cleanup. |
| `refresh_resource_secs` | `[604800, 172800, 21600, 3600]` | Resource refresh intervals by tier (seconds). |
| `refresh_container_secs` | `[86400, 43200, 21600, 3600]` | Container (calendar, feed) refresh intervals by tier (seconds). |
| `refresh_cursor_secs` | `300` | Cursor feed refresh interval (seconds). |
| `change_retention_days` | `90` | Days change lines are retained. |
| `canary_interval_secs` | `21600` | Adapter canary interval (seconds). |
| `web_concurrency` | `4` | Concurrent jobs of the web resolver, which the event routes share. |
| `web_requests_per_second` | `4.0` | Jobs per second of the web resolver and the event routes. |
| `resolver_concurrency` | `2` | Concurrent jobs per pubky or card resolver. |
| `resolver_requests_per_second` | `2.0` | Jobs per second per pubky or card resolver. |
| `resolver_ttl_secs` | `86400` | Default document lifetime (seconds) for web, pubky and card resolvers. |
| `host_requests_per_second` | `1.0` | Requests per second to any one upstream host. |
| `host_rates` | `{}` | Per-host overrides of `host_requests_per_second`. |
| `html_head_bytes` | `4194304` | How far an HTML page past the 1 MB page cap is read (bytes); nothing in the page stops the read earlier. |
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
| `reach_timeout_secs` | `5` | Nexus reach deadline per search (seconds). |
| `related_taggers` | `200` | Taggers a related page samples: the authors of a resource's most recent tags, at most this many. |
| `related_window_days` | `365` | Days a related page looks back, for those taggers and their tags. |
| `related_refs_per_tagger` | `1000` | References a related page reads per sampled tagger (their most recent). |
| `related_min_shared` | `2` | Shared taggers a related row needs (its group's, when collapsed). |
| `related_tag_sample` | `2000` | Tags a related page reads per seed link (its most recent), from which the taggers are sampled. |
| `related_seed_links` | `32` | Seed links a related page samples: of the resource, its group's members and their aliases, the ones with the newest tags, at most this many. |
| `collapse_refs_per_member` | `10000` | References counted per group member when `collapse=group` picks a group's row. |
| `max_radius_m` | `200000.0` | Largest `near` radius (metres). |
| `h3_resolutions` | `[0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 11, 11, 11, 11, 11, 11, 11]` | H3 resolution per map zoom level. |
| `max_cluster_cells` | `10000` | Most cells one cluster query returns. |
| `api_cache_capacity` | `10000` | API response cache entries. |
| `reach_cost` | `6` | Budget units a reach search costs. |
| `related_cost` | `6` | Budget units a related search costs. |
| `preview_requests_per_minute` | `30` | Preview requests per minute per client. |
| `preview_uri_requests_per_minute` | `30` | Preview requests per minute per URI. |
| `preview_cache_capacity` | `10000` | Preview cache entries. |
| `podcast_items` | `1000` | Podcast feed items read per scan. |
| `group_max_members` | `256` | Most members of one automatic group. |
| `group_max_distance_m` | `500.0` | Farthest two located members of one group (metres). |
| `group_max_graph_units` | `20000` | Units one grouping pass may read. |
| `group_max_graph_bytes` | `33554432` | Bytes one grouping pass may read. |
| `also_on_links_per_source` | `32` | Derived links one source keeps per resource. |
| `also_on_keys_per_source` | `16` | Join keys one source keeps per resource. |
| `also_on_facts_per_source` | `32` | Facts one source keeps per resource, at least 8: the cap cuts the many values of one name (a recording's ISRCs) before a name's one value, and 8 holds a recording's single-valued facts (its MBID, the MBID it was merged from, artist, length, original album, barcode). |
| `also_on_concurrency` | `2` | Resources whose sources run at once. |
| `also_on_retry_secs` | `300` | First retry of a failed source run (seconds); doubles up to an hour. |
| `event_max_line_bytes` | `65536` | Longest calendar content line (bytes). |
| `event_max_date_values` | `4096` | Most date values in one calendar property. |
| `event_feed_body_bytes` | `33554432` | Largest calendar or feed body (bytes). |
| `event_max_occurrences_per_series` | `50` | Most occurrences one series expands to. |
| `event_max_occurrences_per_feed` | `10000` | Most occurrences one feed yields. |
| `event_max_components` | `100000` | Most components one calendar may hold. |
| `event_max_revisions` | `10000` | Most revisions of one event (RECURRENCE-ID overrides). |
| `event_max_document_bytes` | `8388608` | Largest retained event document set per feed (bytes). |
| `event_max_recurrence_steps` | `100000` | Most recurrence steps one series may take. |
| `event_max_period_dates` | `100000` | Most dates one period of a recurrence rule may hold; a larger rule is not expanded. |
| `event_parse_timeout_ms` | `2000` | Calendar parse deadline (milliseconds). |
| `event_months_back` | `1` | Months of past occurrences kept in the horizon. |
| `event_months_forward` | `12` | Months of future occurrences kept in the horizon. |
| `geocode_score` | `0.8` | Lowest accepted geocoding score. |
| `held_retry_secs` | `60` | Retry interval of a held feed snapshot (seconds). |
| `feed_max_changes` | `100` | Occurrence changes per snapshot before they become one summary line. |
| `feed_max_rows` | `10000` | Most stored rows of one feed. |
| `feed_max_new_keys` | `1000` | New keys one snapshot may add before it is held. |
| `feed_retention_days` | `30` | Days cancelled and past rows are kept. |
| `feed_max_holds` | `5` | Consecutive holds before a feed dead-letters. |
| `feed_cadence_floor` | `3600` | Shortest feed refresh interval (seconds). |
| `websocket_deadline_ms` | `2000` | Nostr relay websocket deadline (milliseconds). |
| `max_cursor_pages` | `10` | Pages one cursor feed fetch may read. |
| `max_indexer_pages` | `10` | Pages one Nostr indexer lookup may read. |
| `max_derived_resources` | `10000` | Derived links one adapter may hold across all parents. |
| `max_calendar_members` | `50` | Members one NIP-52 calendar may list. |
| `calendar_member_wait_hours` | `48` | Hours a NIP-52 calendar waits for a member that has never resolved. |
| `max_linked_resources` | `100` | Website links one BTC Map feed may derive. |

## Database

Supported: PostgreSQL 16–18, PostGIS 3.4+, `pg_trgm` and h3 4.2.3 or newer (h3-pg added
`h3_latlng_to_cell(point, integer)` in 4.2.3; without it every `cluster=` search fails). After
migrating and before serving, `pinakesd` checks `to_regprocedure('h3_latlng_to_cell(point,integer)')`
and refuses to start without it, naming the h3 `extversion` it found; development and suffixed
builds that have the function start normally. Upgrading the package does not upgrade the
extension in a database, because `CREATE EXTENSION IF NOT EXISTS` keeps the installed version:
run `ALTER EXTENSION h3 UPDATE;` (and `ALTER EXTENSION h3_postgis UPDATE;` if installed).

`migrations/0001_schema.sql` is the whole schema. `pinakesd --config <path> --migrate` creates it
on an empty database and exits; the daemon does the same at startup, and so does `reextract`. A
database that already has it is left as it is.

Until the first deployment there is this one file and no upgrade path: a schema change edits
`0001_schema.sql`, and every local database is then recreated (Pinakes refills it from the Nexus
feed). Tests create their own databases and need nothing. Numbered migrations come back with the
first deployment.

```sh
dropdb pinakes && createdb pinakes
pinakesd --config pinakes.toml --migrate
```

The daemon refuses a database built from another version of the file, naming this remedy.

A database advisory lock, the instance lock, allows one daemon per database. The daemon holds it on
a connection of its own for its whole life. Resolver, host, author, reach-cache and IP budgets are
process-local, and the lock keeps their guarantees. Every daemon version takes the same key, so a
second daemon on a database refuses to start. No other lock uses that key (`pinakes_core::locks`).

**Re-extraction.** A parser or routing change reaches stored documents through one command, run
while the daemon is stopped (it takes the instance lock):

```sh
pinakesd --config <path> reextract <route…|all|also-on>
```

It re-runs the current classification on the selected rows (all rows, or those routed to or
classified as one of the named routes or card kinds) and re-queues them at once, in batches of
1000. Each loses its HTTP validators and, on the OSM route, its `osm_place` cache entry, so the
next fetch re-extracts. A row entering or leaving the OSM route swaps its identity envelope and
becomes pending, as does an OSM-host URL moving to web. Leases are fenced by a generation bump.
Aliases have no route and are not re-queued; one that still holds a failure (an error, a dead
letter, a retry count) gets an alias's state, `pending` (or `ok` if it was read) with none, since
its root answers for it. A row whose URI now has a static alias root (a new tracking parameter,
say) becomes an alias of it. The selected rows' [also-on](#also-on) lookups are
queued too: a card that comes back unchanged would not run them again, so results read by an
older version would stay until they expire. `reextract also-on` queues every resource's also-on
lookups without re-reading any card; it can be combined with routes.

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

**Same-event hint rebuild.** The hourly sweep reads again any event in the window whose start,
place or name changed unseen. To recompute every pair (cards keep their current pairs until each
event is read again):

```sql
UPDATE same_event_unit SET fingerprint='';
INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT resource_id FROM same_event_unit ON CONFLICT DO NOTHING;
```

## Deployment and egress

Web fetches bypass environment proxies, validate every DNS answer, pin the checked addresses and
repeat the policy at every redirect (three hops; HTML meta refresh and literal JavaScript
redirects count). `egress.allow_hosts` and `egress.deny_hosts` take exact names or
`*.example.org`; they never override the IP policy, and deny wins. Pages are capped at 1 MB of
decoded body; an oversized page keeps its head metadata with a `truncated` warning. An HTML page
past the cap is read on to `limits.html_head_bytes`, whole when it ends before that and cut there
otherwise: nothing in the page's text stops the read earlier, since only the parser can say where
a head ends (a `</head>` in a script is not one). A read that cuts stops as soon as it reaches
its cap, so a sender that stalls there costs no timeout. An image or PDF past the
cap is cut too, never refused: its card comes from its URL. Every HTML parse (pages, and HTML
inside API answers) runs in a child process, `pinakesd parse-worker` (the daemon itself; one
that cannot run it does not start). All the parses of one job (a page's extraction, a source's
answer with many HTML descriptions, a calendar's events) share one worker and its limits: 1 s of
CPU time (10 s in a debug build), 1 GiB of memory and a wall-clock deadline. Past any of them, or
as soon as the job's caller stops waiting, the worker dies, every later parse of the job fails
at once, the parse slot is freed, and the resource is recorded as `too_complex`: final, with no
card (an earlier card is cleared, and the change recorded), until `pinakesd reextract` revives it
(the daemon logs `an HTML parse did not end within its limits`, once per job). Text without
markup (no `<`) is parsed in the daemon, in linear time, without a worker, but only while the
job is within its limits: once it is over, it parses no plain text either.
The tree a worker answers with keeps no node more than 256 elements deep and at most 500,000
nodes (of 149 real pages, the most has 37,186). Before the parse, a linear scan cuts the text
at the first obvious cap: 50,000 start tags, 256 open elements, 64 attributes on one tag (end
tags too), an attribute value of 1 MiB, a tag name of 256 bytes (real pages stay far below each:
at most 16,440, 62, 29, 121 KB and 27 bytes over 149 pages). The scan is a cheap early cut; the
worker's limits are the guarantee. At most as many pages are parsed at once as the machine has
cores: a parse holds its slot until it ends, even after its request timed out. An RSS or
Atom feed read without a calendar's feed cap is cut there too (a long show's feed is 2–5 MB):
its channel and newest items come first, and its card reads up to the last whole item. Wikidata
entity reads may take 8 MB, since a much-edited item carries megabytes of claims, and so may
MusicBrainz answers (an artist or release with very many links). An answer over even that is
refused, never cut, and remembered as such for five minutes. The also-on sources then record only
what the link's URL proves (a MusicBrainz page its MBID or ISRC, a Wikidata item its Q-id) and
don't retry it; Wikidata items verified together that are over it together are read one by one;
a MusicBrainz page's card is read from the page. Requests
time out after ten seconds. Images stay origin URLs and are never fetched.

Discovery reads (`Egress::discover`: the also-on sources and hubs, oEmbed endpoints, feed and
calendar discovery, and a few page hooks and sources such as `feed`, `media_records`, Discogs and
GitHub; not the page fetch of web resolution or most official-API reads) share one in-process
cache (10,000 entries, 32 MiB), and concurrent readers of one request share one fetch. A success
is kept for the lifetime the reader asks for, raised to at least an hour; a failure for five
minutes, or, when the service named its wait (a 429's `Retry-After`), for that wait held between 1
and 300 seconds. Eviction of the least recently used entries can end any entry sooner. An
authentication failure (TIDAL's 401 and 403) is never kept, so a new token asks again, and a wait
for Pinakes's own host budget is not a failure.

Every request introduces itself as `pinakes/<version> (+https://pubky.app; <contact>)`, with
`http.contact` as the contact (`info@synonym.to` in the example configuration; without one,
`pinakes/<version> (+https://pubky.app)`). Set it to an address the services you read can reach:
some refuse requests without one, and the links of the sources that need it (SEC EDGAR) wait
until it is set. The User-Agent never names a code-hosting site, which some
services refuse as an undeclared tool. Event platform adapters that need a browser profile keep
their own User-Agent.

The official-API sources ([docs/sources](sources/README.md)) run on the `official` card route
(`[cards.official]`), each with its own switch, `[sources.<name>] enabled` (on by default;
`PINAKES__SOURCES__<NAME>__ENABLED=false`); a switched-off source's links get ordinary web cards
and its address rules no longer merge new links (links it merged before stay merged). A source
that reads with the operator's token (`github`: `[sources.github] token` or `PINAKES_GITHUB_TOKEN`)
is on only with one. Its token is a secret, sent only to its service's API and never through a
redirect. A token the service refuses (401) switches the source off until restart, and
`pinakes_source_token_refusals_total{source}` counts each refusal (Discogs' token too). A source
lowers its service's host to the service's published rate (never above
`limits.host_requests_per_second`) unless `limits.host_rates` names the host, and a request that
would wait more than five seconds for its host (its slot, or the request ahead of it) is retried
later instead of timing out; a redirect within the host keeps the slot it has. After an upgrade
that adds a source, run `pinakesd reextract web official`: links already known get their source's
card.

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
and size (no card, for a kind an app's profile reads). Which objects are fetched and what their
cards hold is per app ([Pubky apps](pubky-apps.md)); after an upgrade that adds or changes an
app's profile, run `pinakesd reextract pubky`. Because the request carries a header, a redirect is refused rather than followed.
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

CORS: `api.cors_allow_origins` (default `["*"]`, safe because the public API takes no cookies or
tokens and writes nothing but budgeted previews) lets browser apps on other origins call the
public listener. List exact `scheme://host[:port]` origins to restrict it (no wildcard patterns
such as `https://*.pubky.app`), or `[]` to send no CORS headers. Preflights get 204 with
`Access-Control-Allow-Methods: GET, POST`, `Access-Control-Allow-Headers: content-type` and
`Access-Control-Max-Age: 600`, and spend the per-IP budget. In `*` mode every response, 429s and
requests without an `Origin` included, carries `Access-Control-Allow-Origin: *` and exposes
`Retry-After` and `ETag`, so a shared cache filled by a non-browser client still serves
browsers. With an origin list only allowed origins get the headers (echoed) and every response
adds `Vary: Origin`. No `Access-Control-Allow-Credentials` is sent, and the admin listener never
sends CORS headers.

The admin listener (`admin_listen`, default `127.0.0.1:9090`) requires `admin_token` (or
`PINAKES__ADMIN_TOKEN`) for a non-loopback bind; terminate TLS at a trusted reverse proxy.

Swagger UI (`api.swagger_ui`, default on) serves the vendored `utoipa-swagger-ui` bundle (Swagger
UI 5.17.14) at `/swagger-ui`; builds never download it, and a build environment must not set
`SWAGGER_UI_DOWNLOAD_URL`. Every `/swagger-ui` response carries `X-Content-Type-Options: nosniff`
and a CSP of `default-src 'self'` with no inline script; the bundle's one inline `<style>` is
allowed by its SHA-256 hash, which a test recomputes from the served bundle. The validator badge
is off. Swagger UI shares the `/v1` per-IP budget.

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
missing for the same period. Conditional responses keep accepted facts, but validators are kept
only for calendars, where they commit with the parsed snapshot.

## Web pages and access walls

Generic web resolution reads JSON-LD, microformats, OpenGraph, oEmbed and the title, then the
page hooks of switched-on sources (WordPress and Ghost records a page names on its own origin, a
media page's object by its exact record; see [docs/sources](sources/README.md)). A hook that
fails or runs out of its three seconds leaves the page's card and logs its source at debug. Canonical, alternate and oEmbed links come from the
document head only. Media pages use the pinned 390-provider oEmbed registry (`data/`, with
SoundCloud, TikTok, Facebook and Instagram skipped; `scripts/oembed-providers` updates it):
`media.oembed = "when_needed"` (default) asks path-specific providers first and broad patterns
only for an incomplete, failed or truncated page; `"always"` also fetches every matching
provider and trusted page-advertised endpoints, which must share the page's origin or its
registry provider. Healthy page JSON-LD stays authoritative and oEmbed fills gaps.
`web.discover_feeds` (web pages) and `cards.media.discover_feeds` (media pages: podcast feed
scanning) follow RSS alternates on pages without a typed card. Discovery decisions are counted
in `pinakes_discovery_decisions_total{kind,decision}`.

A challenge page (a bot wall, captcha or sign-in redirect) is a failed fetch: the resource turns
`unreachable` and is retried with the normal backoff, dead-lettering after
`limits.max_failures`, unless the site's own feed lists the exact link (the
[`feed`](sources/feed.md) wall hook, also for plain 401 and 403 answers) or a source's wall hook
names it (an IMDb title by its Wikidata item, [`media_records`](sources/media_records.md)); then
that is the card. Its last accepted document, event dates and feed occurrences stay, and a wall never counts
toward a disappearance. Fetches that hit one count as outcome `blocked` in
`pinakes_fetch_total`.

The markers are the built-in `data/interstitial-markers.json` (text markers, captcha selectors,
wall host prefixes, sign-in paths). Visible text excludes `script`, `style`, `noscript` and
`template`. Title, heading and captcha markers count only on short pages (under 2,000 visible
bytes) or challenge statuses (403, 429, 503). An HTML answer with status 202 that is empty or
has no visible text is a challenge (AWS WAF answers bots so); an empty 202 of another type is
content. A redirect of the requested path to a listed
sign-in path (`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall unless the
requested path is itself one. A meta refresh without a URL is content; a refresh slower than ten
seconds is a hop only on a short page that does not refresh itself.

## Calendars and events

`[event]` enables event pages (JSON-LD Event types, microformats h-event, ActivityPub Event
negotiation), `event.calendars` the calendar feeds and `event.geocoding` venue geocoding. With
calendars off, feed URLs fall back to web cards. ActivityPub is negotiated once per resolution
for an advertised alternate, an event-looking page without an extracted Event, or an Event
without a zone. An event page can follow a same-host link to its own ICS; discovered URLs share
an LRU cache (10,000 entries, 32 MiB; one-hour minimum TTL, five minutes for failures, or the
wait the answer named when shorter; a 429 that names none keeps five minutes).

Calendar GETs follow the web policy, decompress, and are capped at `limits.event_feed_body_bytes`;
lines are unfolded from that bounded body and parsed on a blocking worker under
`limits.event_parse_timeout_ms`. A malformed VEVENT is skipped with a warning; exceeding a work
or feed cap rejects the snapshot. A recurrence rule is expanded only when one of its periods
holds at most `limits.event_max_period_dates` dates (the recurrence library builds a whole period
before giving its first date); an event with a larger rule keeps DTSTART and its RDATEs, less its
EXDATEs, with the warning `series_rule_unexpanded`. After an upgrade that changes how rules are read, run
`pinakesd reextract event`: feeds re-parse at once instead of at their next fetch. A JSON feed (Gancio, a JSON Feed) is parsed as one document,
so its parsed form can take several times `limits.event_feed_body_bytes` of memory; size that
limit with the worker count in mind. `webcal://` is fetched over HTTPS and never downgrades. A
feed-provided refresh interval is clamped to one hour through 30 days. Held snapshots retry after
`limits.held_retry_secs`, backing off from the normal cadence.

`event.utc_series_wall_clock` (default `"x_wr_timezone"`; also `"coordinates"`, `"never"`)
chooses which zone sources make a series with a UTC DTSTART expand on local wall-clock time
instead of in UTC. The default keeps Google's exports right (UTC DTSTART plus `X-WR-TIMEZONE`)
and leaves feeds whose zone comes only from coordinates in UTC. `event.default_timezone` places
floating times. A change of either applies at each calendar's next re-parse;
`pinakesd reextract event event_platform` applies it at once. Instances such a re-parse moves
get new keys once: the old rows are cancelled after two snapshots, or the feed is held if more
than half its keys move.

Page coordinates win over geocoding, and online events are not geocoded. A structured Nominatim
search is accepted only for one sufficiently matching address (`limits.geocode_score`); an OSM
venue link also needs the name and full address to match. Geocoded facts stay in `osm_place`;
documents store versioned references and compose coordinates at read time.

`event.same_event_hint` (default `"on"`; `"off"`) puts `maybe_same` on event cards: other events
that are possibly the same one ([the rule](api.md#possibly-the-same-event)). A background pass in
the daemon computes it for events starting from a day ago to a year ahead. Triggers queue an event
resource (or a feed) when an event's start, place, name or document changes. Each pass takes up to
25 queued resources and up to 200 of their new or changed events, and the next pass follows at once
while there was work, else after five seconds. An event's normalised name, place and identifiers
are stored once read, and its pairs are matched among the events already read through their start
and geography indexes. An hourly sweep drops events that started over a day ago and queues those
that entered the window, changed unseen, or are no longer events (an alias, a missing feed, a gone
occurrence). Requests only read the stored pairs. `"off"` stops the pass and removes the field from
cards, details and `/v1/openapi.json`; the stored pairs stay, and changes made meanwhile are read
when it is turned back on.

### Event platforms

`data/event-adapters.json` declares each platform's mode, identity, cancellation signal, cadence
floor, expected fields, user-agent profile, key requirement and canary. Configure a platform
under `[event.adapters.platforms.<id>]` with `enabled` and `key`; a missing key disables only
that platform and no key ever appears in `/v1/info`. Resident Advisor (an unofficial GraphQL
API) is off by default; Posh is marked fragile. `event.adapters.enabled = false` sends platform
URLs to the web resolver. The `browser` and `firefox` user-agent profiles imitate browsers to
pass bot walls; consider the provider's terms before enabling those platforms.

Luma, Meetup, Eventbrite, Partiful, Posh, RingCentral, Airmeet, Mobilizon, Gancio, Wikidata,
Dice, Xceed, Ents24 and Ticketino supplement JSON-LD with bounded state parsing; JavaScript is
never executed. Self-hosted Pretix, Eventyay, Gathio, Gancio and Mobilizon are detected from page
signatures, a heuristic that can cost one bounded discovery request. Authenticated APIs never
redirect, and credentials are removed from retained URLs and errors.

Cursor feeds (an event portal, BTC Map, the Google Calendar API) keep their checkpoints in
`event_fetch` and commit them with each accepted batch; an expired cursor resyncs under the
ordinary hold rules. BTC Map requests every delta status and deletion, never advances its cursor
past the local fetch time, and allows five minutes of clock skew. Malformed cursor rows are
skipped (`invalid_cursor_row`, drift) and never imply deletions. With
`event.adapters.derive_resources` (default on), NIP-52 calendar members and BTC Map event
websites are queued as resources, charged to the referring authors (or a per-adapter sponsor)
within `limits.max_calendar_members`, `limits.max_linked_resources` and
`limits.max_derived_resources`. A BTC Map link expires with its event, capped at the horizon
and extended at each feed apply as the horizon reaches it (a link that lapsed at the cap comes
back while its event is ahead). Each link records which feed rows name it: a website shared by
two events keeps the farther instant, and a tombstoned or re-linked row stops extending it;
a NIP-52 member link ends when the calendar stops listing the member and otherwise lives 24 hours
from the calendar's last fetch. A lapsed link keeps its child through the orphan grace
(`limits.grace_days`) counted from the link's expiry, so a feed that failed for a day deletes no
member, and a revived link meets the same per-parent and global caps as a new one.
Portal coordinates are page facts, never written to `osm_place`.

Nostr identifiers and client URLs use signed NIP-52 events through two HTTP indexers (Wine at one
request per second, Archives at two), paginating `until` windows within
`limits.max_indexer_pages`; the newest matching revision wins, and `nevent`/`note` pin one. Both
must miss before absence is accepted. With `event.adapters.websocket`, one relay hint is asked
after both miss (`limits.websocket_deadline_ms`, one MB, 256 frames, DNS checked and pinned); an
unfinished subscription never confirms absence. A full tied page, an exhausted budget or an
indexer 404 is inconclusive and retries after five minutes without charging an attempt.

Only parser failures fall back to a web card. Upstream failures keep the accepted document and
retry; a refresh never replaces a dated Event with a start-less card. Registry-listed secondary
denials and GraphQL failures defer, and after three days become `unreachable` with ordinary
backoff. HTTP 451 and cursor 401/403/404 follow two-observation disappearance. Direct Wikidata
404/410 do too, and a missing entity never removes a live Wikipedia article.

`cargo run -p pinakes-core --example adapter_probe -- MANIFEST.json REPLAYS.json OUTPUT_DIR`
(with `DATABASE_URL`) runs a read-only, keyless canary pass and records requests, user-agent
profiles, statuses, hashes and parser output; secrets are refused. `PINAKES_PROBE_OFFLINE=1`
replays captures without network access. Recorded responses for the keyless probes run in
`scripts/test`.

## Also on

After setting a source's key (`[podcast_index]`, TIDAL's pair, a Discogs token), or after an
upgrade that adds or changes also-on sources, run `pinakesd --config <path> reextract also-on`
(or `reextract all`, which includes it): until then links already stored keep the results of
their last run for up to the source's lifetime (30 days). After an upgrade that changes how links
are spelled, typed or named (aliases of a canonical link, a card's service in `site`), run
`reextract all`: stored cards keep what the older build gave them until each is fetched again.

The also-on worker runs every enabled source that applies to a resource ([the API](api.md#also-on))
after a save that changed its document or status, and again when the shortest lifetime among
them has passed. Its queue is `also_on_dirty`; `limits.also_on_concurrency` (2) resources run at
once, and a failed run keeps the previous rows and retries after `limits.also_on_retry_secs`
(300 s), doubling up to an hour, or after the wait a service asked for when that is longer. Each run replaces its source's rows (`resource_key`,
`derived_link`, `resource_fact`); rows live twice the source's lifetime and the hourly maintenance deletes the
expired ones (a deleted key regroups its resource). A source keeps at most
`limits.also_on_links_per_source` (32) links, `limits.also_on_keys_per_source` (16) keys and
`limits.also_on_facts_per_source` (32) facts per resource. `[also_on] enabled = false` stops the
worker; stored keys keep grouping and stored links stay on cards until they expire (up to twice
the sources' lifetimes). `derive_links = false` keeps keys and facts only: no derived links on
cards, so a tagged link an exact derivation names no longer joins by it either;
`[also_on.sources.<id>] enabled = false` turns one source off and `enabled = true` turns on one
that is off by default (an unknown ID stops startup). Sources run in a fixed order, and each
sees the keys the ones before it found in the same run (a barcode MusicBrainz names, then the
services looked up by it); a source that no longer applies (its key is gone) drops its rows. A
source that fails or panics keeps its earlier rows and retries with backoff; the others' results
stand and the worker goes on (a panic is logged as an error). A source that asks a secondary
service besides its own (DataCite and Crossref for a paper, ProtonDB, PCGamingWiki and GOG for a
game, the provenance reads and deps.dev for a package, Software Heritage for a repository, the
Federal Register API, Discogs' record for a release) keeps what the link and the other services
gave when that service fails: the link's own key is recorded, the failure is counted in
`pinakes_also_on_service_errors_total{service,class}`, and the run comes back after a failed
run's backoff instead of after the source's lifetime: five minutes, doubling to an hour while the
service keeps failing, or the wait the service named when longer. A run in which every service
answers resets it.
The sources, in the order they run, are `service`, `tidal`, `musicbrainz`, `musicbrainz_entity`,
`performers`, `wikidata`, `discogs`, `deezer` and `itunes` (music), `wikipedia`, `wikidata_item`, `books`, `films`, `podcasts`
and `titles`, `osm_wikidata`, `wikidata_osm` and `btcmap` (places), `federal_register`, `unpaywall`,
`ncbi`, `datacite` and `huggingface` (papers), `crates`, `npm`, `pypi` and `software_heritage`
(software), `steam` and `wikidata_stores` (games and apps) and `wayback` (archived copies)
([the API](api.md#also-on) says what each reads); `[also_on] apple_storefront` (`us`, a
two-letter country code) is the Apple Music, Apple Books and Apple Podcasts storefront of the
Apple links they build. `[also_on.sources.musicbrainz] base_url`
(`https://musicbrainz.org/ws/2`) points every MusicBrainz read at a mirror's web service instead
(the `musicbrainz`, `musicbrainz_entity` and `performers` sources, and the cards of MusicBrainz
pages), an http(s) URL without a trailing slash and reached through the egress policy;
every instance on one public address shares MusicBrainz's 1 request/s, which a mirror does not
limit. The worker takes up to 100 due resources at a time and asks MusicBrainz about their links'
URLs together (50 to a request; MusicBrainz echoes each URL, and an answer applies only to the URL
it names) and Wikidata for their item links' items together (10 to a request). Each music link then
costs one MusicBrainz request for its entity (up to four when the URL names several releases; one
more for a track found by its ISRC, up to two barcode searches for an album found by its barcode keys (only those another source
found) and up to four release reads for what they name, and one for a
track's original album), up to two iTunes lookups
for an album with barcodes, and two Wikidata requests (a search by its service ID cannot batch);
each Wikipedia article one request to its Wikipedia and one to Wikidata; each MusicBrainz page
one request for its entity (an ISRC's page one more for the ISRC; MusicBrainz has no lookup of
several entities at once, and a read the card already made is taken from the cache) and one for a
recording's original album; each event page naming performers (up to 16, with 8 links each, within
2 KB of the document) one MusicBrainz URL request for all its performers' readable links together
and no other (an artist's page on a music service, Songkick or Bandsintown; at most two per
performer; a link the answer leaves out is unknown until the next run, and a busy or failed request
fails the run of a page on a performer link's service, which retries, while another page records
what it says without MusicBrainz and asks again after MusicBrainz's retry time; answers are cached
an hour, so an artist's many concerts naming only
that artist, by one link, ask once), and none for a performer the page names by a MusicBrainz
artist page alone; each tagged OSM object one Wikidata request; each book one Open Library
request, one iTunes lookup for an ebook edition, and two Wikidata requests when Open Library names
its work; each podcast show one iTunes lookup, two or three Podcast Index requests (the last asks
who owns the show's GUID), its feed (usually still cached from its card) and two Wikidata requests;
each episode up to three Podcast Index requests, two iTunes lookups and, for an episode's page, its
feed; each page whose head names an RSS feed that feed (cached for an hour, read up to 8 MiB), and
Podcast Index only when one item of it with audio or video is that page; each film or series two
Wikidata requests (a TVmaze show first its TVmaze record), and a Letterboxd and a TVmaze request
when the item has no link there; each TMDB, Trakt, Netflix, Goodreads, Letterboxd or IMDb page
(`titles`) the item's search and read (usually cached already) and one request for its English
label; each DOI one request each to Unpaywall, the NCBI ID Converter and DataCite, and one to
Crossref when DataCite names its preprint (an arXiv paper one to DataCite, and one to Crossref when
DataCite names its journal version); each package
one registry request, a PyPI package up to two more for its files' provenance, an npm package one
more for its attestations, and one to deps.dev when the registry names no provenance; each Steam game
up to four (ProtonDB, PCGamingWiki, GOG GamesDB, GOG); each store link two Wikidata requests; each
gone page one Wayback CDX request; each Discogs release one Discogs request (the one its card
makes), and a Discogs master one Wikidata request (the item MusicBrainz relates its release group
to). Requests that several sources make (an item, a record) are
shared through the discovery cache. `btcmap` keeps BTC Map's place list in memory, brought up to
date at most hourly from its sync (`updated_since`, 2,000 places a page); after a restart the
first lookup reads it again. `unpaywall` and `ncbi` need the contact setting (`[http] contact`)
and find nothing without it. `deezer` is off by default: Deezer's API terms allow
non-commercial use only, so turn it on
(`[also_on.sources.deezer] enabled = true`) only where they do; it costs one request per Deezer
link and up to two per other track or album with an ISRC or barcode. `tidal` costs a token
request when its token expires (a minute before TIDAL's `expires_in`), and per TIDAL album or
track one request, or a share of one: a batch
of links asks for up to 20 albums or 20 tracks at once (`filter[id]`); a track's ISRC also asks
MusicBrainz whether several recordings have it (usually cached from `service`).

TIDAL is asked only with a TIDAL developer app's client ID and secret (developer.tidal.com):
`PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`, the recommended form (one
underscore: a build without the source ignores them, where `PINAKES__TIDAL__CLIENT_ID` stops it),
or `[tidal] client_id` and `client_secret`. Without both the `tidal` source does not run;
credentials removed later leave the rows it stored until they expire, as `enabled = false` does.
`[tidal] country` (`US`, an ISO 3166-1 alpha-2 code in upper case; another value stops startup)
is the country whose catalogue is read. The pair
and the token never appear in logs, the operator info, errors or debug output. Refused
credentials (the token endpoint refuses them, the API answers 403, or it rejects a token under a
minute old) are logged (`TIDAL refused the client credentials`) and counted in
`pinakes_also_on_service_errors_total{service="tidal"}` (class `refused`, `forbidden` or
`rejected`); the TIDAL links read meanwhile keep what TIDAL gave them before and retry later
(`tidal_auth`, after the five-minute hold, then backing off to hourly), and TIDAL is asked again
when the hold ends. A rate limit
or an outage (429, 5xx) is waited out: no TIDAL request, batched or single, before the time it
asked for (`Retry-After`, a minute to an hour) has passed, and the links retry after it
(`tidal_busy`; a failure the cache replays meanwhile reports the same). TIDAL's answers are
cached by the credential pair, not the token, so a new token keeps them, and a batch asks only
for the items that have no cached answer.

Podcast Index is asked only with a key and secret: `[podcast_index] key` and `secret`, or
`PINAKES__PODCAST_INDEX__KEY` and `PINAKES__PODCAST_INDEX__SECRET` (free from
api.podcastindex.org). Without them a show joins through its Apple ID only: no GUID is a key, a
feed gets no Apple ID, a Podcast Index page has nothing to read, and episodes join only by their
Apple track. The key and secret never appear in logs, the operator info or errors. A refused key
or a Podcast Index outage fails no lookup: the source keeps what the other services answered,
and asks again later only when nothing at all was found. Such failures of a secondary service
(Podcast Index, iTunes, Wikidata, the feed) are counted in
`pinakes_also_on_service_errors_total{service,class}`.

Sources reach their services through the egress policy and its per-host budgets, one request
at a time per host. A hub's published limit lowers its host's default pace like a source's own
(`limits.host_rates` names a host's rate outright): MusicBrainz 1 request/s, Deezer 10/s, TIDAL's API 2/s (TIDAL publishes no figure),
Wikimedia (`www.wikidata.org`, `*.wikipedia.org`, `commons.wikimedia.org`) 200/min, Open
Library 3/s, iTunes about 20/min, TVmaze 20 per 10 s, crates.io 1/s, Crossref 10/s, NCBI's ID
Converter 3/s, Software Heritage 120/h, the Wayback Machine one request every 2 s (its CDX API asks
for it; the pace covers the host, so a tagged capture's card waits behind the lookups too); a limit
above `limits.host_requests_per_second` (1/s) leaves the default.

## Groups

Grouping runs after a document commits, from its own dirty queue with its own backoff (5–10
seconds of jitter, growing to an hour), so a grouping failure never blocks a save. A document
records its evidence as group keys: an identifier card's `doi`/`isbn` and the landing pages its
registry names (`registry`), a page's head citations (`cites_doi`, `cites_isbn`) and ICS UIDs
(`ics_uid`); the exact keys also-on sources vouch for join too (`hub:{key}`, below). Groups form
only from those (see [the API](api.md#groups)): an identifier card with
a named landing page that cites it, one UID within one feed, and curation. A group stores
membership only; cards read it when they are served. Each key of a run may
make a capped number of compatibility checks (`limits.group_max_members`² × 2, between 100,000
and 2,000,000). A key that alone exceeds the cap is parked (its unions undone) and the later
keys still run; the run settles and counts it
(`pinakes_group_budget_resources{state="parked"}`). A run stops at four caps and retries within
the hour with `last_error = 'work_budget'` (`state="stopped"`), starting at the key it stopped
at, so a component too large for one run is covered in turn; after 24 stopped runs on unchanged
input it settles with the groups the last run left until its input changes (`state="settled"`).
A join that would put two values of a single-object hub key in one group (two recordings, two
Q-ids) is refused; `pinakes_group_key_conflict_resources` counts the resources whose last run
refused one, which points at a wrong ID on a hub.
Parked keys' checks count against the run. Past either limit, every set of units holding one the
run did not finish keeps each member's group, and no other set takes that group's ID; in a
stopped run so does every set that `limits.group_max_members` kept from joining another, since a
resumed run meets the keys in another order. A member deleted by orphan expiry leaves its group
at once. Writer lock contention retries after one to three seconds; curation waits up to two
seconds and answers 503 with Retry-After. `groups.cross_container_occurrences = "corroborated"`
also joins one UID across feeds when the instances are within 500 m, 300 s and a name similarity
of 0.5; a determined publisher can copy that, so it is off by default.

Tenant rules (`data/multi-tenant-hosts.json`) say which part of a multi-tenant host is one
publisher; the canonical [alias](api.md#aliases) rule needs the same tenant. A rule is
`"subdomain"`, `"first_path_segment"`, `{"path_segment":N}` or
`{"path":{"segment":N,"skip":["u/*"],"extend":["s"],"query":["src"]}}`: `skip` drops a noise
window (Google Calendar's `u/{n}`), `extend` takes the next segment too (anchor.fm `/s/{id}`),
and `query` scopes by a parameter when the path is short (`embed?src=`).

## Monitoring

`GET /metrics` on the admin listener serves Prometheus series per source: fetch outcomes,
changes, canaries, the queue, ingestion lag and grouping. Admin `GET /v1/info` has the
manifests, resolver status (aliases left out: their roots answer for them), counts (`aliases`
among them), ingestion, source and canary state; public `/v1/info` shows
only the version, what the instance resolves, the limits a client must respect and the OSM
licence. Stored errors are sanitized classes (dns, connect, timeout, http_4xx, http_5xx, parse,
policy, too_large for an answer past its size cap, too_complex for a page whose parse did not
end within its limits; rate_limited for a link waiting for its host's budget). A failed resolve
is logged at warn as `resolution failed`, with the resource id (`resource_id`), the link's host
(`host`; its scheme for a link without one, never its path), this class (`kind`) and the
upstream HTTP status when one was answered (`status`), never the error's text. No log line about
a fetch, a lookup or a parse prints an error's text (what an upstream answered may hold a link);
the lines about the daemon's own services (the database, Nexus, the dump) print theirs through
a last guard that cuts every URL before its query or fragment and drops its user info. Logs name
a link only by its host (a calendar's token or a user's name in a link never reaches a log line),
errors carry no URL (a fetch error or a Nexus request error names none, an upstream service's
error text is not repeated, the API answers an invalid link with the reason, not the link, and
a query or body it cannot read with what the route takes, never the parameter name or value it
could not read). Admin info and database metrics are cached for 15 seconds.

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
