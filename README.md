# Pinakes

A Pubky index of external resources: the web pages, places, events, feeds, media and records
that people reference on Pubky.

Nexus records references to external links and never fetches them. Pinakes reads each link into
a small typed card, joins the same thing across services, and answers one search: text, type,
map area, time, tags and reach.

Clients combine the two by URI: **Pinakes ranks, Nexus hydrates.** A client sends the URIs of the
cards Pinakes returns (`uri`, and `aliases` for other spellings; for an occurrence,
`occurrence.feed_uri`) to Nexus `/v1/stream/resources/by_ids` as `uris` (batches of 100) for
counts and tags. Pinakes IDs are its own and never match Nexus IDs.
Pinakes reads the Nexus `/v1` API: the reference feed and that batch view (pubky/pubky-nexus
[#1189](https://github.com/pubky/pubky-nexus/pull/1189) and
[#1185](https://github.com/pubky/pubky-nexus/pull/1185)), the reach-set route
([#1232](https://github.com/pubky/pubky-nexus/pull/1232)), and each viewer's `following` and
`friends` lists. Nexus serves the first three under `/v1` only.

## How it works

```
Nexus  GET /v1/resources/changes          reference feed: tag, reply, embed, collection, post
  │
  ▼
ingest     resource row per URI, one row per reference             ingest.rs, identity.rs
  │
  ▼
queue      one job per resource: route, lease, backoff, refresh     queue.rs, scheduler.rs
  │
  ▼
resolvers  web, pubky, geo, osm, event, event_platform, cards       resolver/, event/
  │        official APIs (the "official" card route)                sources/
  ▼
card       typed document, change journal                           store.rs, changes.rs
  │
  ├── aliases     one object under several URIs                     alias.rs
  ├── groups      one thing under several URIs, from exact keys     groups.rs, group_keys.rs
  ├── also-on     keys, facts and links from hubs and services      also_on.rs, also_on/,
  │                                                                 lookups/, music_sources.rs,
  │                                                                 hubs.rs
  └── same-event  events that are possibly the same one             same_event.rs
  ▼
/v1 API    search, cards, lookup, resolve, changes                  api.rs, change_api.rs
```

Paths are under `crates/pinakes-core/src/`. `crates/pinakesd` is the daemon: it parses the
command line, creates the schema, and starts the listeners and the background tasks (ingest,
resolver workers, also-on, grouping, same-event, maintenance, the ODbL dump).

**Ingest.** `pinakesd` polls the Nexus reference feed. Each line names a URI; Pinakes normalizes
it and hashes it into its own resource ID (`identity.rs`), stores one reference row per
reference, and queues a resource it has not seen. A tag is keyed by its author, link and label
(the file a tag line names is for information only); a reply, embed or collection item by its
author, post file and link; a post by its version file. Nexus decides which references exist;
Pinakes never parses the social specs.

**Resolve.** `resolver::classify` routes each URI by scheme and host, without network. The queue
leases rows per route and runs that route's resolver under its own concurrency and rate limits. The
web resolver reads JSON-LD, microformats, OpenGraph, oEmbed and the title. The other routes read
`pubky://` objects of other apps, `geo:` URIs and map links, OpenStreetMap objects (through
Nominatim), event pages and calendar feeds, event platforms (`data/event-adapters.json`), and media,
ISBN, DOI, Nostr and ActivityPub cards. Over forty services with their own API (Federal Register,
SEC EDGAR, Bluesky, GitHub, Discogs, Zenodo, …) are read from that API instead of their pages; each
has a page in [docs/sources](docs/sources/README.md). The result is one document per resource: a
schema.org type, name, description, image, site, point, start and end, and the rest a read shows. A change to a
stored document becomes a line in the change journal.

**Join.** An alias is the same object under another spelling (tracking parameters, OSM URL forms,
ISBN-10); it reads as its root resource. A group joins resources that are one thing, only on
evidence a page cannot fake: a DOI or ISBN card and the landing page its registry names, one ICS UID
within a feed, an operator's curation, and exact keys from the also-on sources. The also-on worker
asks hubs (MusicBrainz, Wikidata) and services about their own records (TIDAL, Discogs, iTunes, Open
Library, Podcast Index, crates.io, Steam, the Wayback Machine, …) for music, books, films, podcasts,
places, papers, software, games and apps, and stores keys (`isrc:…`, `mb:recording:…`,
`wikidata:Q…`, `doi:…`), facts and derived links. Only a hub, or a service's own record of its own
ID, makes a key; what a page says about itself is at most a fact. A group is a hint on each card,
never a merge.

**Serve.** The public listener (`listen`, port 8090) serves `/v1`: one search with filters,
facets and group collapse; cards by ID or URI; batch lookup; a compose-time
`resolve`; feed occurrences; and changes since a time. The admin listener (`admin_listen`, port
9090) serves `/metrics`, operator info, stored documents and group curation.

**Database.** PostgreSQL with PostGIS and `pg_trgm`. The schema is one file,
`migrations/0001_schema.sql`. The main tables are `resource` (the document and its indexed facts),
`reference`, `occurrence`, `resource_alias`, `resource_group`, `resource_key`, `resource_fact`,
`derived_link` and `resource_change`. The work queues are tables too: `resource` rows for
resolution, `group_dirty`, `also_on_dirty` and `same_event_dirty`. OpenStreetMap-derived facts
live only in `osm_place`, which is published under ODbL as a dump file. The current index can be
rebuilt by replaying the Nexus feed and fetching every link again; operator group curation
(`group_override`) and the change journal (`resource_change`) cannot, so back them up. Production
schema upgrades require a reviewed in-place procedure; disposable local databases can be recreated
([operations](docs/operations.md#database)).

**Egress.** Every fetch a reference can steer goes through one client (`resolver/egress.rs`):
public HTTP(S) only, every DNS answer checked and pinned, private and metadata addresses refused,
at most three redirects (each checked again), a 10-second timeout and a 1 MB body cap (an HTML
page is read on to `limits.html_head_bytes`, 4 MB by default; 8 MB for MusicBrainz and Wikidata
answers). Requests to one host go one at a time, paced to
`limits.host_requests_per_second` or the host's published rate. Discovery reads (also-on
lookups, oEmbed, feed discovery) are cached and shared between concurrent readers. Card images
stay origin URLs and are never fetched; a link that is itself an image resolves to an ImageObject
card. Nominatim and Nexus are trusted upstreams outside this policy.

## Run locally

Requires Rust 1.98.1 (`rust-toolchain.toml` selects it), PostgreSQL 16–18 with PostGIS 3.4+ and
`pg_trgm`. On Ubuntu: `postgresql-16 postgresql-16-postgis-3 postgresql-contrib-16` (from the
[PGDG repository](https://www.postgresql.org/download/linux/ubuntu/) if the distribution lacks
version 16). Docker is not required.

```sh
sudo pg_ctlcluster 16 main start
sudo -u postgres createuser --superuser "$USER"
createdb pinakes
cp pinakes.example.toml pinakes-dev.toml
scripts/dev
```

`scripts/dev` migrates the database, starts `pinakesd` on port 8090 (admin on 9090) and tails
`.runtime/pinakesd.log`. Point `nexus_url` at a Nexus that serves the resource changes feed. The
local superuser is for development only: in a deployment a superuser creates the database and
its extensions, and the daemon connects as the database's owner, no superuser
([operations](docs/operations.md#database)).

```sh
curl http://127.0.0.1:8090/v1/info
curl 'http://127.0.0.1:8090/v1/search?tags=rust&facet=tags&limit=10'
curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' \
  http://127.0.0.1:8090/v1/resolve
```

Swagger UI is at `http://127.0.0.1:8090/swagger-ui`, with an example for every kind of card and
source.

`pinakesd` commands:

- `pinakesd --config <path>` runs the daemon (and creates the schema on an empty database);
- `pinakesd --config <path> --migrate` creates the schema on an empty database and exits;
- `pinakesd --config <path> reextract <route…|all|also-on>` re-reads stored documents after a
  parser or routing change, with the daemon stopped
  ([operations](docs/operations.md#database));
- `pinakesd --openapi` prints the OpenAPI document.

## Test

```sh
# The role needs CREATEDB and permission to create the PostGIS extension.
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
scripts/test
```

`scripts/test` runs `cargo fmt --check`, clippy with warnings denied, every workspace test, the
e2e driver's own Node tests (`npm test --prefix e2e`, so it needs Node 20 or newer) and
`scripts/check-ops`. Integration tests create their own disposable databases and never touch the
development database. HTTP tests use local servers, injected transports and recorded fixtures.
The tests that run `scripts/verify-osm-boundary` also need `psql`, `pg_dump` and `rg`.

End-to-end checks ([e2e/README.md](e2e/README.md)) need Node 20 or newer too:

- `scripts/e2e --mock` runs the whole flow (schema, ingest, queue, resolvers, groups, API)
  against fixtures, offline;
- `scripts/e2e --isolated-live` writes real tags through the Pubky SDK, checks them through a
  real Nexus and public services, and deletes them after;
- `scripts/demo-lookup [--base URL]` resolves a curated list of public links against a running
  daemon and fails when one misses its expected type, status or derived links. It writes nothing
  to Pubky or Nexus, but `POST /v1/resolve` creates and queues each link in that Pinakes.

## Configuration

Choices live in the TOML file ([`pinakes.example.toml`](pinakes.example.toml)): addresses, one
`enabled` switch per resolver, source and feature, event platforms, egress allow and deny lists,
and the public API's budget and CORS. Every cap, timeout and refresh tier has a default and can
be set in an optional `[limits]` section; the table is in [docs/operations.md](docs/operations.md).

Secrets come from the environment. Any key can be set as `PINAKES__SECTION__KEY`
(`PINAKES__WEB__ENABLED=false`, `PINAKES__LIMITS__POLL_SECS=10`): strings are literal; numbers,
booleans and arrays are JSON. The environment wins over the file, and unknown keys or invalid values
stop startup. [`.env.example`](.env.example) lists the secret variables with placeholder values: the
database URL, admin token and all optional provider credentials, including Spotify, TIDAL,
Discogs, GitHub and CourtListener. The [API-key guide](docs/api-keys.md) explains signup,
minimum permissions and key-free fallbacks. Copy it to `.env`, fill in what you use,
and load it (`set -a; . ./.env; set +a`, systemd `EnvironmentFile=`, or `docker run --env-file`).
Keep `.env` out of version control and readable by the service user only.

Set `[http] contact` to an address the services you read can reach. It goes into the User-Agent,
`pinakes/<version> (+https://pubky.app; <contact>)`; some services (SEC EDGAR) refuse requests
without one.

## Documentation

- [docs/api.md](docs/api.md): the `/v1` API, response shapes and example requests.
- [docs/operations.md](docs/operations.md): limits, database, egress, re-extraction, monitoring
  and maintenance.
- [docs/deployment.md](docs/deployment.md): services to host, measured storage baseline,
  sizing estimates, production network and upgrade checklist.
- [docs/api-keys.md](docs/api-keys.md): complete credential setup and provider fallbacks.
- [docs/sources](docs/sources/README.md): one page per official-API source.
- [docs/pubky-apps.md](docs/pubky-apps.md): the Pubky app profiles and how to add one.
- [ops/README.md](ops/README.md): metrics, alerts and the Grafana dashboard.
- [data/README.md](data/README.md): pinned data sets and their licences.
- [e2e/README.md](e2e/README.md): the end-to-end harness.
- [AGENTS.md](AGENTS.md): how to work on the code.
