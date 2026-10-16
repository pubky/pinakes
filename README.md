# Pinakes

A Pubky index of external resources: the web pages, places, coordinates,
event pages, feeds and media that people reference on Pubky.

Nexus records references and never fetches. Pinakes reads Nexus's reference
feed, resolves each URI into one small typed document through a resolver per
kind, keeps the facts a query needs beside the public references, and answers
one search: text, type, map area, time, tags and reach. The rule for clients is
**Pinakes ranks, Nexus hydrates**.

The design is in [`docs/design.md`](docs/design.md). Maps and Events are the
first clients; each kind of client adds a resolver and its own logic on the
same base.

## Status

The Rust service, Postgres storage, Nexus ingestion, web/Pubky/geo/OSM resolvers and
viewport API, change tracking, generic events/calendars and admin telemetry run locally.
Platform event adapters include cursor feeds, NIP-52, embedded page state, per-event ICS and optional keyed APIs. Media and groups follow on a separate branch.

## Running locally

Requires Rust (2021 edition; the current Pubky SDK requires Rust 1.89 or newer),
PostgreSQL **16**, PostGIS, `pg_trgm` and `h3`. Docker is not required. On Ubuntu, install
`postgresql-16 postgresql-16-postgis-3 postgresql-contrib-16 postgresql-16-h3`; if version 16 is not
in the distribution, use the [official PGDG repository](https://www.postgresql.org/download/linux/ubuntu/).

For a disposable local development environment:

```sh
sudo pg_ctlcluster 16 main start
sudo -u postgres createuser --superuser "$USER"
createdb pinakes
cp pinakes.example.toml pinakes-dev.toml
scripts/dev
```

The local superuser is needed to create PostGIS extensions and disposable test
databases. Use separate migration and restricted application roles for a deployed
instance. `scripts/dev` applies sqlx migrations, starts `pinakesd` on port 8090,
and tails `.runtime/pinakesd.log`; Ctrl-C stops it. The local config and logs are
ignored by Git. Set `nexus_url` to a Nexus with the resource changes feed. This
workspace's authorized staging relay is `http://127.0.0.1:8080`; it is read only.

Every config value can be overridden with `PINAKES__SECTION__KEY`, for example:

```sh
PINAKES__WEB__ENABLED=false scripts/dev
PINAKES__LISTEN=127.0.0.1:8091 scripts/dev
PINAKES__EGRESS__DENY_HOSTS='["*.example.org"]' scripts/dev
```

Strings are literal env values; numbers, booleans and arrays use JSON syntax.
Unknown config keys and invalid budgets stop startup. See
[pinakes.example.toml](pinakes.example.toml) for all settings. Do not commit database
passwords; inject them with `PINAKES__DATABASE_URL` when needed.

```sh
cargo run -p pinakesd -- --config pinakes-dev.toml --migrate
curl http://127.0.0.1:8090/v1/info
curl 'http://127.0.0.1:8090/v1/search?tags=rust&facet=tags&limit=10'
curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' \
  http://127.0.0.1:8090/v1/resolve
```

## API and architecture

[API reference](docs/api.md) describes the implemented `/v1` subset. Utoipa derives
the OpenAPI served at `/v1/openapi.json`; regenerate the checked-in snapshot with
`scripts/openapi`. Search filters complete reference data before sorting or paging;
clients pass its ordered IDs to Nexus `/v0/stream/resources/by_ids` for social
hydration, in batches of 100. Pinakes does not hydrate or fetch through Nexus.

`pinakes-core` contains the schema, feed client, resolver contract, queue and API.
`pinakesd` runs migrations, the HTTP server, feed polling and resolver workers.
The shared identity crate is copied **unchanged** from the supplied Nexus extraction;
it will become a git dependency on pubky-nexus after that PR merges. No social
specs are parsed here. The pinned schema.org vocabulary and its separate license
are documented in [data/README.md](data/README.md).

`geo:` URIs resolve locally to final places. Canonical OSM object URLs use batched
Nominatim lookup (up to 50 IDs/request, 30-day cache). OSM-derived fields live only
in `osm_place`, under ODbL; a non-materialized SQL view joins them for reads and
search. Pubky references keep their independent IDs and data. The public
`/v1/dumps/osm_place.ndjson` stream and `pg_dump --table osm_place` export the
complete derivative database. OSM documents, cards and clusters carry attribution.

Search supports bounding boxes (including the antimeridian), distance in metres,
time windows and H3 viewport clusters. PostgreSQL filters, ranks, aggregates tags
and clusters before limiting the response. `h3o` validates the zoom table, with a
PostgreSQL integration test comparing every supported cell resolution. Event search
includes external pages with starts, calendar occurrences and located Pubky event
references. `venue=<resource_id>` selects events at a known place; cards carry times
and occurrence/post handles for the client to hydrate.

```sh
curl 'http://127.0.0.1:8090/v1/search?bbox=5.9,45.8,10.6,47.9&cluster=8&type=Place&facet=tags'
curl 'http://127.0.0.1:8090/v1/search?near=46.0051,8.9528&radius=1000&sort=distance'
```

The default development upstream is the public Nominatim. Its
[usage policy](https://operations.osmfoundation.org/policies/nominatim/) requires
an identifying User-Agent, caching and one thread, with an absolute maximum of
one request/second. This service defaults to 0.5 requests/second and never proxies
place search, autocomplete or reverse geocoding. The one-time development run is
51 user-referenced objects in two calls. Configure your own `osm.nominatim_url` for
ongoing/staging/production use; recurring public bulk work must stay below four
requests/minute. OSM API fallback is disabled by default and has the same serialized
budget when explicitly enabled, for nodes only (no way/relation `/full` downloads). Requests have a ten-second timeout, no redirects,
no environment proxy, gzip and a two-MB decoded response cap. URLs are operator
configuration, never supplied by the caller.

## Verification

```sh
# This role needs CREATEDB and permission to create the PostGIS extension.
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
scripts/test
# Read-only acceptance check and optional ODbL-only SQL export of a resolved database.
DATABASE_URL='postgresql:///pinakes?host=/var/run/postgresql' scripts/verify-osm-boundary .runtime/osm_place.sql
```

`scripts/test` runs fmt, clippy with warnings denied, and all workspace tests. sqlx
creates separate disposable databases for integration tests; no test uses or resets
the development database. Tests require a real PostgreSQL server and fail explicitly
if it is unavailable. Egress tests use local servers and injected transport/DNS
backends; the production IP policy has no localhost bypass.

## Operations

The durable queue leases rows with `SKIP LOCKED` and fences stale completions by a
generation counter. A single job lease lasts at least 60 seconds; a 50-object batch gets 760 seconds.
Refresh cadence follows the manifest and time to start; no-start web/Pubky pages
refresh after seven days, OSM after its configured cache TTL. `geo:` is final.
Two fresh successful missing observations confirm cancellation; a cached copy
cannot count twice. The first missing result schedules a confirmation with a
one-hour floor. Transient
failures back off exponentially and enter dead letter after five failures. Last good
facets survive failed refreshes. A lookup or preview prioritizes an overdue resource inside its existing TTL,
author budget and retry schedule. It cannot revive a dead letter. A new public
reference can reopen a final non-geo resource.

A database advisory lock permits one daemon per database. Resolver, host, author,
reach-cache and IP budgets are process-local; the lock keeps their guarantees intact.
Web connections bypass environment proxies, validate all DNS answers, pin checked
addresses, and repeat policy checks at every redirect. Host rules never override IP
blocks. The 1 MB cap applies to decoded body bytes. Discovery permits one additional
oEmbed JSON fetch within the original ten-second resolution deadline. It cannot
replace an already extracted field. Images remain origin URLs.

Nominatim, the OSM editing API and Nexus are trusted, operator-configured upstreams.
Their HTTP clients deliberately do not apply web DNS pinning or private-address refusal
(Nexus defaults to loopback). Configure these URLs and their DNS through trusted operator
controls. Upstream Retry-After delays are still capped at one hour.

Pubky deployment: before any SDK fetch or TCP probe, Pinakes resolves the owner's PKARR
HTTPS endpoints, including delegated homeservers and the SDK's `/info` discovery target.
It refuses empty resolutions or any endpoint with a non-public address, checking every
DNS answer for ICANN endpoints; ports are unrestricted. The preflight shares the SDK's
PKARR cache and ten-second resolution deadline. **Run Pinakes with an egress firewall
that denies private, loopback, link-local and metadata ranges. Set `[pubky] enabled = false`
if you cannot enforce that boundary.** Preserve explicitly required access to trusted
operator services, such as the local Nexus/database, in that deployment policy.

Pinned-source audit: Pubky 0.16.2's `actors/storage/resource.rs::to_transport_url` constructs
HTTPS URLs; direct endpoints use PKARR raw-public-key TLS (`pkarr` 8.1.0 `extra/tls.rs`).
However, `client/http_targets/native.rs::build_transport_request` accepts ICANN fallback
with ordinary CA-validated TLS in production, without pinning it to a homeserver key.
`client/core.rs::build` retains reqwest 0.13.5's default redirects and `https_only=false`:
an HTTPS response can redirect to plaintext HTTP outside testnet. Thus the transport is
not universally pinned TLS, and private destinations cannot be assumed to be only blind
handshake probes. These findings supersede the earlier RPK-only deployment note.

The SDK exposes no connection/address-validation hook; its HTTP clients remain private.
The preflight is not connection pinning: a PKARR record republished between validation
and the SDK's resolution, changed DNS answers, cached transport choices, redirects and
ambient SDK proxy behavior can escape it. The pinned SDK exposes no connection DNS override: changing `/etc/hosts` would be a process-global side effect and cannot pin PKARR endpoints or all redirect targets. Pinakes therefore does not mutate it. ICANN DNS is resolved again by the SDK after the check; the checked addresses are not its connection addresses. The firewall must enforce the boundary
independently of the SDK. Public info exposes no transport diagnostics.

Preview limits use the socket peer IP; forwarded headers are deliberately ignored.
If deploying behind a reverse proxy, its shared peer IP gets the aggregate budget;
configure the edge's own client-IP limits. `/v1/info` includes the last observed
feed length and epoch, queue status counts, dead-letter counts and resource counts.
A Nexus outage leaves cached data available and polling retries every interval.

For an operator-directed retry of a dead letter, use a database transaction:

```sql
UPDATE resource SET dead_letter=false, attempts=0, next_attempt_at=now(),
  lease_until=NULL, generation=generation+1 WHERE id='<resource_id>';
```

A feed epoch change (or requested cursor beyond the feed length) clears the reference
projection and scores, retains resource documents during the grace period, and starts
at zero. Feed authors must be exactly 52 lowercase z-base-32 characters. Optional labels
and object paths must be nonempty and NUL-free, bounded in UTF-8 bytes by
`ingest.max_label_bytes` (default 128, maximum 1024) and `ingest.max_object_path_bytes`
(default/maximum 2048). A bad line is skipped without truncating keys or modifying
references; its cursor position still advances. `pinakes_ingest_rejected_total{reason}`
counts the first rejected field after the batch commits, with fixed reasons `author`,
`label`, `object_path`; rollback/retry does not inflate it. This counter resets on restart.
Cursor/identity/timestamp errors still fail the transaction. Existing rows are preserved.
Resources with no references expire seven days after becoming orphaned;
preview-only entries follow the same policy. Search excludes unreferenced resources.

## Refresh, changes and monitoring

Every adapter declares its mode, cadence floor, identity, cancellation signal,
expected fields and optional canary. `/v1/info` exposes these declarations and
the public summary. Full source state and metrics are on admin `/v1/info`. Both info
and database-backed metrics are cached for 15 seconds. Near-start resources refresh more often (7 days, 2 days, 6 hours,
1 hour); one successful fetch at/after end+24h makes the resource final. Container
cadence is driven by its earliest upcoming occurrence or feed TTL. The container
key-set guard holds losses over half the set until an identical successful result
confirms them, then applies changes per key.

`/v1/changes`, `/v1/changes/lookup` and `/v1/resources/{id}/changes` expose normalized
changes: created, rescheduled, relocated, cancelled, uncancelled, renamed, details
and deleted. Fetch timestamps, TTL and body length do not generate changes.
Failures preserve the last accepted document. Records are retained 90 days by
default; sequence allocation is serialized with commits so a reader cannot skip
a lower uncommitted sequence. OSM history remains inside `osm_place.versions`;
change records reference those versions, and reads join them with attribution.

Prometheus is served **only** on `admin_listen` (`127.0.0.1:9090` by default):

```sh
curl http://127.0.0.1:9090/metrics
curl 'http://127.0.0.1:8090/v1/changes?cursor=0&limit=100'
scripts/check-ops
```

Canaries run every six hours, first scheduled one interval after startup. They
share the existing resolver workers, host serialization and request rates. Probe
results update only metrics/source state, never documents, OSM cache or change
records. Their due time, lease, result and failure streak survive restart. A bad
parser produces `outcome=drift` when an otherwise successful result lacks a declared
field. Each actual upstream request is counted once; OSM batches and cache hits do
not multiply request counts by the number of resources.

See [ops/README.md](ops/README.md) for alert rules, the Grafana dashboard, counters
and scrape setup. `scripts/test` checks every alert/dashboard metric against the
registry and runs `promtool check rules` when available (`apt install promtool`
on this host). No alert contact point is provisioned by this repository.

Public `/v1/info` includes only version, counts, ingestion cursor/epoch/length/lag/time,
resolver status, manifests and the OSM licence. It never exposes the Nexus URL or source
errors. Stored source errors are restricted to dns/connect/timeout/http_4xx/http_5xx/
parse/policy classes. Generic web pages declare no expected fields, since images and
pages without titles are valid; platform adapters can opt into drift detection.
Denied web canaries are skipped, and each worker checks due canaries at most once/minute.

`[web]`/`[pubky] ttl_secs` supplies the parser document's initial TTL; scheduling is
controlled by `[refresh]`. Conditional responses preserve accepted facts, but validators
are not persisted for ordinary web/Pubky pages. Calendar validators commit atomically with parsed snapshots. Conditional headers are
suppressed until an initial snapshot is saved, while missing keys or a held/missing feed
need confirmation, and when the persisted `horizon_end` is no more than half the forward
window ahead of the observation clock. That periodic unconditional GET re-expands a
static recurring feed; otherwise a 304 renews TTL without an additional GET.

Public queries use a separate pool with five-second PostgreSQL statement/acquisition
limits, and all `/v1` routes have a configurable direct-peer IP budget. Search revisions
are maintained on writes; ETags include the current cached reach set. Large cluster
views coarsen to `search.max_cluster_cells`, while fractional zooms are accepted. OSM
dumps release database connections and permits on per-chunk or total deadline expiry.
Their read-only transactions locally disable the five-second SQL statement timeout;
the streaming deadlines still bound slow readers, without changing other API queries.
Reach pagination has one `search.reach_timeout_secs` deadline (default five seconds),
with no network I/O under the shared cache lock. Reach-enabled searches cost
`api.reach_cost` budget units (default six), including cache hits. Search cursors use
HMAC-BLAKE3 with a random key generated at daemon startup; restart pagination after a
restart or an invalid-cursor response. The key is neither configured nor persisted.

Use `pinakesd --config <path> --migrate` for upgrades. It prepares any legacy web-fetched
canonical OSM rows before migration 0002 reclassifies them, preserving all historical
SQLx migration checksums. Forward review migrations clear alias metadata/history and
install the stricter Place-envelope constraint. This path is tested from both the
step 2 schema and the reviewed step 3 schema.

## Generic events and calendars

`[event]` controls JSON-LD Event/subtypes, microformats h-event, ActivityPub Event
negotiation and calendar containers. Missing fields are filled in that order;
OpenGraph alone remains a WebPage. ActivityPub is negotiated for an advertised alternate, an event-looking page with no extracted Event, or an extracted Event lacking a zone. Each resolution permits one cached negotiation request. An Event page can follow a same-host link to its own ICS; discovered URLs share an LRU cache (10,000 entries / 32 MiB). Successful responses have a one-hour-minimum TTL; failures use five minutes. Eviction can shorten the TTL, with every new fetch still subject to host budgets. Platform adapters extend this path below. A calendar can run
with web disabled; its workers share the web host budget and the OSM lookup mutex.
With calendars disabled, feed URLs fall back to the enabled web module and its
page-sized card extraction; stored routing hints remain valid when re-enabled.

ICS, event RSS/Atom and Gancio JSON feeds become DataFeed resources with occurrence
rows. Calendar GETs use the normal pinned-IP/redirect policy, decompression and a
32 MB cap; physical ICS lines are read incrementally from that bounded body, unfolded,
and parsed on a blocking worker. A malformed VEVENT is skipped with a warning; exceeded work or feed caps reject the snapshot. Conditional requests pause during disappearance/hold confirmation; after confirmation, 304 responses renew the accepted snapshot without a second GET. HTTPS downgrade is disabled by default; `event.webcal_http_fallback` permits it only for `webcal://` and an explicit TLS-protocol mismatch, never `webcals://`, policy errors, certificate errors or timeouts. The two permitted TLS-protocol error messages are
matched narrowly; a reqwest/rustls wording change fails closed.
The default horizon is one month back/twelve forward, with 50 instances per series
and 10,000 per feed. At most 10,000 event revisions are retained (`max_event_revisions`).
The revision map and expanded occurrence set each have an 8 MiB document budget
(`max_event_document_bytes`); database upserts encode at most 256 rows per chunk. RSS/Atom/JSON event items are converted directly into
bounded occurrence rows, with item, document-memory and parse-deadline checks. JSON
decoding first materialises one DOM from the capped body on the blocking pool. Mixed
feeds keep dated items and warn about skipped undated entries. A feed-provided refresh
interval is clamped to one hour through 30 days.
See the example config for caps and individual path switches.

Each `(feed, UID, recurrence_id)` keeps its own version, cancellation state and change
history. Two distinct successful observations confirm disappearance; an over-50% loss
holds the snapshot until a matching fresh fetch. A former feed returning HTML 200 or HTTP 404/410 follows the same two-observation
rule; confirmed missing parents hide their occurrence units from search while retaining
direct history. A fresh valid body can restore them. The first vanished-feed observation
is a held empty snapshot: it increments `feed_held_total` and may trigger `FeedHeld`,
just like a large key loss; the next matching observation resolves it. Older/replayed
snapshots are fenced.
Ended instances freeze after 24 hours; horizon roll-off does not cancel them. A URI in
an occurrence links only to a different existing resource on the same host or an
explicitly configured `event.container.link_hosts` host, without an unsolicited fetch.
URL-less instances have no link to their own feed. Regenerated UIDs supersede the
old key without a cancellation; the new creation carries `supersedes`.

`[event.container]` limits changes, total retained rows, and new keys per refresh.
Changes contain field patches; a burst over the cap becomes one summary with
occurrence and parent counts, plus the parent change class and fields. Repeated
identical key growth can confirm a legitimate addition; the total row cap stays hard.
Consecutive holds back off exponentially from the normal cadence and dead-letter
after five attempts by default. Bookkeeping lives in `event_fetch`, with aggregate
`source="event"` metrics. Hourly maintenance removes cancelled/superseded and
past-horizon rows after end + 30 days (configurable). Occurrence reads use signed
keyset pagination (100 default, 500 maximum) and default to the feed horizon.
Spatial and text queries use geometry/geography and stored tsvector indexes.

Page coordinates win. Online events skip geocoding. A structured Nominatim search is
accepted only for a single sufficiently matching address; an OSM venue link additionally
requires the name and full address to match. Geocoded facts remain in `osm_place`;
resource/occurrence/change documents store versioned references and compose coordinates
at read time. Both lookup and venue requests use one in-flight mutex and public-host
rate clamp. Staging/production must configure their own Nominatim, as for OSM lookups.

`status` remains fetch health; `event_status` carries scheduled/cancelled/postponed/
rescheduled/moved_online. Offsets do not imply an IANA zone. Zones come from explicit
AP/ICS metadata or the pinned coordinate table; unzoned floating times retain
`start_local`/`end_local`, with UTC columns only a sorting proxy. Clients should use
local values when `tz` is absent. Data versions, attribution and regeneration are in
[data/README.md](data/README.md); the complete client contract is in [docs/api.md](docs/api.md).

The companion design inputs are `design-events.md` §3 and `research-event-sources.md`
§A/F in the delivery mailbox. The repository keeps its overview in `docs/design.md`.


## Platform event adapters

`data/event-adapters.json` declares each source's mode, identity, cancellation,
cadence floor, expected fields, UA, key requirement and canary. `/v1/info` reports the
effective enabled state and reason when disabled, without exposing keys. All adapter
options live under `[event.adapters.platforms.<id>]`; missing keys disable only that
source. Resident Advisor is unofficial, fragile and off by default. Posh is marked
fragile. Class C sources remain cards with a seven-day floor, including error retries.
Disabling adapters sends their HTTP resources to the enabled web resolver; Nostr
resources wait until their module is enabled. Migration 12 updates historical routing
hints without changing resource IDs or reference rows.

Luma, Meetup, Eventbrite, Partiful, Posh, RingCentral, Airmeet, Mobilizon, Gancio,
Wikidata, Dice, Xceed, Ents24 and Ticketino supplement generic JSON-LD with bounded
state parsing; JavaScript is never executed. ICS endpoints are selected from the
current event identity or its own calendar link. Self-hosted Pretix/Eventyay/Gathio/
Gancio/Mobilizon are detected from page signatures. RingCentral Rails zones use the
pinned Rails 8.1.0 mapping and licence in `data/`. Browser/plain/non-Chrome profiles
are fixed choices, configured per adapter. Every HTTP redirect, API endpoint and
WebSocket hint uses the shared DNS/IP/host budget policy; authenticated APIs never
redirect, and configuration credentials are removed from retained final URLs and errors.

The portal and Google API cursors resync when expired. BTC Map requests all delta
statuses and deletions without a server row limit, avoiding timestamp-tie truncation;
an oversized response fails without advancing. Checkpoints live in `event_fetch`
per resource and commit atomically with accepted occurrence batches. `source_state`
keeps only finite adapter-level health and reset counters. Delta absence is not a
deletion. Portal changes preserve deletion/recreation order; explicit portal OSM
facts are stored in `osm_place` with versioned venue references. BTC Map `website`
links enqueue their own page resources; the occurrence links to that resource.

NIP-19 identifiers and supported client URLs use signed NIP-52 events. The newest
matching revision wins (smallest event ID on a timestamp tie); nevent/note IDs pin a
specific revision. Wine is limited to one request/second, Archives to two. Both must
successfully miss before absence is accepted; outages preserve the last document.
One relay hint can provide a bounded REQ/EOSE exchange (two seconds by default,
one MB/256 frames). DNS is checked and pinned, and an unfinished subscription cannot
confirm absence. Kind 31924 calendars queue their capped child references, then read
accepted child documents; they never multiply inline network fetches. RSVP kind
31925 is ignored. HTTP discovery and relay results share the bounded TTL cache.

Run `cargo run -p pinakes-core --example adapter_probe -- MANIFEST.json REPLAYS.json OUTPUT_DIR` with `DATABASE_URL` for a read-only, keyless canary pass. Manifest entries
are `{id,uri}`; a null URI records a skip. Replay entries contain `{uri,status,
content_type,path}` (or `error`). Captures record exact request URLs, UA profiles,
statuses, hashes and parser output; secrets are refused. Set `PINAKES_PROBE_OFFLINE=1`
to replay without DNS/network fallback; uncaptured responses fail closed. Canaries that are unpublished,
blocked, too large, or have no public sample are reported explicitly. A live probe
never enables geocoding or writes calendar members. Recorded compressed responses for 45 keyless probes, synthesized keyed/API shapes,
and mock cursor/relay tests run in `scripts/test` without public upstream access.

The calendar `held_retry_secs` option is an additional hold retry floor (up to seven days); normal cadence and the one-hour minimum still apply before exponential backoff. Embedded VTIMEZONE transitions share the parser deadline and enforce `max_date_values` per expansion. Enabling `webcal_http_fallback` emits a startup warning about plaintext integrity.


Adapter review behavior: cursor URI variants share a canonical dataset container and checkpoint;
resource identities and references retain their original URI. Portal history checkpoints each
bounded page batch and takes a full snapshot after reaching the head, including after expiry.
BTC Map timestamp cursors allow five minutes of clock skew; portal sequences are numeric,
monotonic and bounded, with implausible future-sized values rejected rather than committed.
Initial Google Calendar syncs use the configured horizon. Public `/v1/info` shows only canary
hosts; the admin listener retains full URLs. Over-budget canary samples are inconclusive
(`canary_ok = null`), retried at the next interval, and still count as transport errors.

`event.adapters.derive_resources` defaults to true: NIP-52 calendar members and BTC Map
website pages count against the referring authors (or a bounded per-adapter sponsor), with
per-parent and per-adapter caps. BTC Map sponsorships expire with the event; historical
events cannot permanently exhaust admission. Set false to disable this additional work. Portal-supplied OSM
fields are untrusted page coordinates, never writes to `osm_place`; an independently referenced
OSM URL resolves through Nominatim normally. No `osm_id` or version is attached to those facts.

The `browser` and `firefox` user-agent profiles impersonate browsers to pass bot walls;
operators must consider the provider's terms before enabling them. NIP-52 `D` tags are only
checked for the legacy warning; their value is not read or used for event identity. Self-hosted
HTML substring detection (`gancio`, `mobilizon`, `/export/event/`) is a heuristic and can cause
an unnecessary bounded discovery request; it never evaluates embedded JavaScript.

Nostr HTTP lookups paginate inclusive `until` windows (up to `max_indexer_pages` per
indexer within ten seconds). A full tied page, ignored window or exhausted budget remains
inconclusive and retries without consuming failure attempts; a search-endpoint 404 is not
an authoritative absence. Portal cold starts read its head cursor before the full snapshot.
BTC Map cursors never advance beyond the local fetch time, even when an upstream clock is fast.

Only parser failures trigger the platform web-card fallback. Upstream failures, including
cached discovery failures, retain the accepted document and retry. Optional venue geocoding
records `venue_unavailable` and keeps the event; accepted venue data is preserved.
Denied secondary requests and GraphQL failures defer, while Wikidata 404/410 follow
two-observation disappearance. Nostr reserves relay time inside the queue budget and
retries incomplete scans after five minutes, reusing cached pages. The captured Guidle
calendar request is now correctly replayed as an egress failure (its HTML card used to hide
that failure). Wikipedia entity enrichment requires event/place JSON-LD or infobox cues;
ordinary articles stay web cards without a speculative Wikidata fetch.
