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
viewport API run locally. Event extraction and change tracking follow separately.

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
PostgreSQL integration test comparing every supported cell resolution. Occurrences
have a table but no producer or route yet.

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
budget when explicitly enabled. Requests have a ten-second timeout, no redirects,
no environment proxy, gzip and a two-MB decoded response cap. URLs are operator
configuration, never supplied by the caller.

## Verification

```sh
# This role needs CREATEDB and permission to create the PostGIS extension.
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
scripts/test
```

`scripts/test` runs fmt, clippy with warnings denied, and all workspace tests. sqlx
creates separate disposable databases for integration tests; no test uses or resets
the development database. Tests require a real PostgreSQL server and fail explicitly
if it is unavailable. Egress tests use local servers and injected transport/DNS
backends; the production IP policy has no localhost bypass.

## Operations

The durable queue leases rows with `SKIP LOCKED` and fences stale completions by a
generation counter. A single job lease lasts at least 60 seconds; a 50-object batch gets 760 seconds. Successful
resolutions refresh after their TTL; missing web/Pubky objects use the shorter TTL, and OSM negative results use the OSM cache TTL. `geo:` has no TTL and never refreshes. Transient
failures back off exponentially and enter dead letter after five failures. Last good
facets survive failed refreshes. A preview shares the same resource/queue entry as
ingestion and cannot revive a dead letter or bypass the TTL.

A database advisory lock permits one daemon per database. Resolver, host, author,
reach-cache and IP budgets are process-local; the lock keeps their guarantees intact.
Web connections bypass environment proxies, validate all DNS answers, pin checked
addresses, and repeat policy checks at every redirect. Host rules never override IP
blocks. The 1 MB cap applies to decoded body bytes. Discovery permits one additional
oEmbed JSON fetch within the original ten-second resolution deadline. It cannot
replace an already extracted field. Images remain origin URLs.

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
at zero. Resources with no references expire seven days after becoming orphaned;
preview-only entries follow the same policy. Search excludes unreferenced resources.
