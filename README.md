# Pinakes

A Pubky index of external resources: the web pages, places, coordinates, event pages, feeds and
media that people reference on Pubky.

Nexus records references and never fetches. Pinakes reads Nexus's reference feed, resolves each
URI into one small typed document through a resolver per kind, keeps the facts a query needs
beside the public references, and answers one search: text, type, map area, time, tags and
reach. The rule for clients is **Pinakes ranks, Nexus hydrates**: clients pass the URIs of the
cards Pinakes returns (`uri`, and `details.aliases` for other spellings; for an occurrence,
`occurrence.feed_uri`, since its own URL was never tagged) to Nexus
`/v0/stream/resources/by_ids` as `uris` (batches of 100) for social state. Pinakes IDs are its
own: the two combine by URI, never by ID.

- [docs/design.md](docs/design.md): what Pinakes is and the decisions behind it.
- [docs/api.md](docs/api.md): the `/v1` API, response shapes and a catalogue of example requests.
- [docs/operations.md](docs/operations.md): configuration, limits, deployment and maintenance.
- [ops/README.md](ops/README.md): metrics, alerts and the Grafana dashboard.
- [data/README.md](data/README.md): pinned data sets and their licences.

## What it resolves

- **Web pages**: JSON-LD, microformats, OpenGraph, oEmbed and title, with access-wall detection.
- **Pubky objects** of other apps, from conventional JSON members.
- **Places**: `geo:` URIs and map links offline; OpenStreetMap objects through batched Nominatim
  lookups, kept in an ODbL derivative table.
- **Events**: event pages, ICS/RSS/Atom/JSON calendar feeds with recurrence, event platforms
  (Luma, Meetup, BTC Map and others in `data/event-adapters.json`) and NIP-52 calendars.
- **Cards**: media (oEmbed, podcasts), ISBN (Open Library), DOI (CSL), Nostr and ActivityPub.
- **Official APIs**: keyless APIs that answer exactly where a page is walled or thin, such as
  Federal Register documents by number ([docs/sources](docs/sources/README.md)).
- **Aliases and groups**: spellings of one object (tracking parameters, OSM URL forms, ISBN-10)
  are one resource; a DOI or ISBN and the landing page that cites it, or one ICS UID within a
  feed, form a group: a hint on each member's card, never a merge (search shows one row per
  group only with `collapse=group`).

It also keeps a change journal, so clients can ask what changed for the links they cached.

## Running locally

Requires Rust 1.89 or newer, PostgreSQL **16–18** with PostGIS **3.4+**, `pg_trgm` and h3
**4.2.3+** (`pinakesd` refuses to start without `h3_latlng_to_cell(point, integer)`). Docker is
not required. On Ubuntu, install
`postgresql-16 postgresql-16-postgis-3 postgresql-contrib-16 postgresql-16-h3` (from the
[PGDG repository](https://www.postgresql.org/download/linux/ubuntu/) if the distribution lacks
version 16).

```sh
sudo pg_ctlcluster 16 main start
sudo -u postgres createuser --superuser "$USER"
createdb pinakes
cp pinakes.example.toml pinakes-dev.toml
scripts/dev
```

`scripts/dev` creates the schema, starts `pinakesd` on port 8090 (admin on 9090) and tails
`.runtime/pinakesd.log`. Point `nexus_url` at a Nexus with the resource changes feed. The local
superuser is only for development; a deployment uses a migration role and a restricted
application role.

```sh
curl http://127.0.0.1:8090/v1/info
curl 'http://127.0.0.1:8090/v1/search?tags=rust&facet=tags&limit=10'
curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' \
  http://127.0.0.1:8090/v1/resolve
```

Swagger UI is at `http://127.0.0.1:8090/swagger-ui`, with examples for every kind of card and
source, each named by its value.

`pinakes.example.toml` lists the operator choices: addresses, credentials, one `enabled` switch
per resolver and feature, event platform keys, egress lists and the public API's budget and
CORS. Every cap, timeout and refresh tier has a default in an optional `[limits]` section,
documented in [docs/operations.md](docs/operations.md).

Pubky objects are fetched through the same egress policy as web pages, from the homeserver's
public HTTPS domain; see [operations](docs/operations.md#pubky).

## Configuration and secrets

Choices live in the TOML file; secrets and per-deployment overrides live in the environment.
Any key can be set as `PINAKES__SECTION__KEY` (`PINAKES__WEB__ENABLED=false`,
`PINAKES__LIMITS__POLL_SECS=10`): strings are literal, numbers, booleans and arrays use JSON, and
the environment wins over the file. A platform's key
(`PINAKES__EVENT__ADAPTERS__PLATFORMS__TICKETMASTER__KEY`) and a host rate
(`PINAKES__LIMITS__HOST_RATES__<HOST>`) create their entry. Unknown keys and invalid values stop
startup, so a misspelt variable is an error, not a silent default.

Set `[http] contact` (`PINAKES__HTTP__CONTACT`) to an address the services you read can reach:
it goes into the User-Agent, `pinakes/<version> (+https://pubky.app; <contact>)`. Some services
refuse requests without it; links to them (SEC EDGAR) wait until it is set. An address naming
github is refused, since some services block such User-Agents.

[`.env.example`](.env.example) lists every variable this version reads for secrets (the database
URL with its password, the admin token, the platform keys) with fake values. Copy it to `.env`,
fill in what you use and load it:

- a shell: `set -a; . ./.env; set +a`, then start `pinakesd`;
- systemd: `EnvironmentFile=/etc/pinakes/pinakes.env` in the unit;
- a container: `docker run --env-file .env …`.

Keep `.env` out of version control (it is in `.gitignore`) and readable by the service user only;
never commit real keys, tokens or passwords.

## Schema

The schema is in `migrations/` (`0001_extensions.sql`, `0002_schema.sql` and the numbered
changes after it);
`pinakesd --config <path> --migrate` builds or migrates a database. A parser or routing change
reaches stored documents through `pinakesd --config <path> reextract <route…|all>`.

## Layout

- `crates/pinakes-core`: schema access, the Nexus feed client, resolvers, the queue, grouping,
  the change journal and the API.
- `crates/pinakesd`: the daemon: migrations, the two HTTP listeners, feed polling and workers.
- `data/`: the pinned schema.org vocabulary, oEmbed registry, event adapter manifests, wall
  markers, multi-tenant host rules, time zone tables and the API examples.
- `e2e/`: the end-to-end harness and the read-only demo lookup.
- `ops/`: Prometheus alerts and the Grafana dashboard.

## Verification

```sh
# The role needs CREATEDB and permission to create the PostGIS extension.
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
scripts/test
```

`scripts/test` runs fmt, clippy with warnings denied, every workspace test and
`scripts/check-ops`. Integration tests create disposable databases on a real PostgreSQL server
and never touch the development database. Egress tests use local servers and injected
transports; the production IP policy has no localhost bypass. The tests that run
`scripts/verify-osm-boundary` also need `psql`, `pg_dump` and `rg`: under `scripts/test` they
fail without them, and a bare `cargo test` skips that check with a message.

- `scripts/e2e --mock` runs the end-to-end flow against fixtures; `scripts/e2e --isolated-live`
  writes real tags through the Pubky SDK and deletes them after. See [e2e/README.md](e2e/README.md).
- `scripts/demo-lookup [--base URL] [--file e2e/samples.lookup.json]` previews a curated list of
  public URIs through `POST /v1/resolve` against any running daemon, prints kind, URI, type,
  site, status and group, and exits 1 when one fails, stays pending or misses its expected type
  (an ancestor matches) or status. It is read-only and paced under the preview budget.
- `scripts/verify-osm-boundary [out.sql]` checks a resolved database keeps OSM-derived facts
  only in `osm_place`, and can export that table.
- `scripts/openapi` regenerates the OpenAPI snapshots in `docs/`.
- `scripts/lock-waits COMMAND...` runs a command while it samples the `DATABASE_URL` server
  every 50 ms, then prints how many samples saw a backend waiting on a lock and which locks
  they waited for (instance, write order, alias, change sequence, group, ingest, search
  revision, row).
  The `write_load` test runs eight writers and an ingest (tags added and removed) against one
  database and fails if one waits on the search revision or deadlocks; `WRITE_LOAD_ROUNDS`
  (default 15) scales it. The `daemon` test runs the `pinakesd` binary on a database of its
  own: it saves a resolved link while it holds the instance lock, then a re-extraction returns.
