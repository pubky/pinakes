# Pinakes

Pinakes is the Pubky index of external resources: the web pages, places, events, feeds, media and
records that people reference on Pubky. It reads each referenced link into a small typed card,
joins the same thing across services, and serves one search over all of them: text, type, map
area, time, tags, identifiers and reach.

## Who is this for?

- **Client authors** building on Pubky: read [How Pinakes works](docs/design.md), then the
  [API reference](docs/api.md).
- **Operators** running an instance: start with [Run locally](#run-locally), then
  [deployment](docs/deployment.md), [operations](docs/operations.md) and
  [API keys](docs/api-keys.md).
- **Developers** working on Pinakes: read [How it works](#how-it-works), the
  [source pages](docs/sources/README.md) and [AGENTS.md](AGENTS.md).

## Pinakes and Nexus

[Pubky Nexus](https://github.com/pubky/pubky-nexus) indexes the social graph: who tagged, replied
to, embedded or collected which link. It records references to links and never fetches them.
Pinakes reads the links.

Clients combine the two by URI: **Pinakes ranks, Nexus hydrates.** A client asks Pinakes for cards
and sends the cards' URIs (`uri`, the `aliases` that are other spellings of it, and for a calendar
date `occurrence.feed_uri`) to Nexus `POST /v1/stream/resources/by_ids` as `uris`, in batches of
100, for counts and tags. Pinakes IDs are its own and never match Nexus IDs.

Pinakes needs these Nexus `/v1` routes:

| Nexus route | Used for |
| --- | --- |
| `GET /v1/resources/changes` | The reference feed: every tag, reply, embed and collection item on a link |
| `POST /v1/stream/resources/by_ids` | Social data for the `social` tool that agents use |
| `GET /v1/user/{id}/following`, `…/friends`, `…/reach-set` | `reach` and `boost` in search |

## How it works

```
Nexus  GET /v1/resources/changes      reference feed: tag, reply, embed, collection, post
  │
  ▼
ingest      one resource per link, one row per reference           ingest.rs, identity.rs
  │
  ▼
queue       one job per resource: route, lease, backoff, refresh   queue.rs, scheduler.rs
  │
  ▼
resolvers   web, pubky, geo, osm, event, event_platform, cards      resolver/, event/
  │         official APIs, one module per service                   sources/
  ▼
card        typed document and its change journal                  store.rs, changes.rs
  │
  ├── aliases     one object under several URIs                    alias.rs
  ├── groups      one thing under several URIs, from exact keys    groups.rs, group_keys.rs
  ├── also on     keys, facts and links from hubs and services     also_on.rs, also_on/, lookups/
  └── same event  events that are possibly the same one            same_event.rs
  ▼
/v1 API     search, cards, lookup, resolve, occurrences, changes   api.rs, change_api.rs
```

Paths are under `crates/pinakes-core/src/`. `crates/pinakesd` is the daemon: it parses the command
line, creates the schema, and starts the listeners and the background tasks.

1. **Ingest.** `pinakesd` polls the Nexus reference feed. Pinakes normalizes each link, hashes it
   into its own resource ID, stores one row per reference, and queues a link it has not seen.
2. **Resolve.** Each link goes to one route, chosen from the link alone. The web route reads
   JSON-LD, microformats, OpenGraph, oEmbed and the title. Other routes read OpenStreetMap objects,
   `geo:` points and map links, event pages and calendars, other Pubky apps' objects, ISBN, DOI,
   Nostr and ActivityPub links, and the links of services whose own API Pinakes reads instead of
   their pages. The result is one card: a schema.org type, name, description, image, site, place,
   times and facts.
3. **Join.** Hubs (MusicBrainz, Wikidata) and services are asked about each card's object, by exact
   identifiers only. Their answers give keys (`isrc:…`, `mb:recording:…`, `wikidata:Q…`, `doi:…`),
   facts and derived links. Resources that share a key form a group. A group is a hint on each
   card, never a merge.
4. **Serve.** The public listener (port 8090 by default) serves `/v1`. The admin listener (port
   9090) serves `/metrics`, operator info, stored documents and group curation.

Pinakes stores its data in PostgreSQL with PostGIS and `pg_trgm`. The whole index can be rebuilt by
replaying the Nexus feed, except operator group curation and the change journal, which need a
backup. [How Pinakes works](docs/design.md) explains every step and its rules.

## Run locally

You need:

- Rust 1.98.1 (`rust-toolchain.toml` selects it);
- PostgreSQL 16 to 18 with PostGIS 3.4 or newer and `pg_trgm`. On Ubuntu, install
  `postgresql-16 postgresql-16-postgis-3 postgresql-contrib-16`, from the
  [PGDG repository](https://www.postgresql.org/download/linux/ubuntu/) if your distribution lacks
  version 16;
- a Nexus that serves the `/v1` resource changes feed.

Docker is not required.

1. Create the database and a configuration file:

   ```sh
   sudo pg_ctlcluster 16 main start
   sudo -u postgres createuser --superuser "$USER"
   createdb pinakes
   cp pinakes.example.toml pinakes-dev.toml
   ```

2. In `pinakes-dev.toml`, set `nexus_url` to your Nexus, and `[http] contact` to an address the
   services Pinakes reads can reach you at. The contact goes into the User-Agent,
   `pinakes/<version> (+https://pubky.app; <contact>)`; some services (SEC EDGAR) refuse requests
   without one.

3. Start the daemon:

   ```sh
   scripts/dev
   ```

   `scripts/dev` builds `pinakesd`, creates the schema, starts the daemon on port 8090 (admin on
   9090) and tails `.runtime/pinakesd.log`.

4. Send a first query:

   ```sh
   curl http://127.0.0.1:8090/v1/info
   curl 'http://127.0.0.1:8090/v1/search?tags=rust&facet=tags&limit=10'
   curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' \
     http://127.0.0.1:8090/v1/resolve
   ```

   Swagger UI is at `http://127.0.0.1:8090/swagger-ui`, with an example for every kind of card and
   source.

The local superuser is for development only. In a deployment, a superuser creates the database and
its extensions, and the daemon connects as the database's owner
([operations](docs/operations.md#database)).

### Commands

| Command | What it does |
| --- | --- |
| `pinakesd --config <path>` | Runs the daemon, and creates the schema on an empty database |
| `pinakesd --config <path> --migrate` | Creates the schema on an empty database and exits |
| `pinakesd --config <path> reextract <route…\|all\|also-on>` | Reads stored links again after a parser or routing change; run it with the daemon stopped ([operations](docs/operations.md#database)) |
| `pinakesd healthcheck` | Asks the admin listener's `GET /healthz`; exits non-zero unless it answers 200 |
| `pinakesd --openapi` | Prints the OpenAPI document |

## Configuration

Settings live in one TOML file; [`pinakes.example.toml`](pinakes.example.toml) lists them with
comments. It holds the listen addresses, one `enabled` switch per resolver, source and feature,
event platforms, egress allow and deny lists, and the public API's budget and CORS. Every cap,
timeout and refresh interval has a default and can be set in an optional `[limits]` section
([operations](docs/operations.md#configuration)).

Any key can also be set in the environment as `PINAKES__SECTION__KEY`, for example
`PINAKES__WEB__ENABLED=false` or `PINAKES__LIMITS__POLL_SECS=10`. Strings are literal; numbers,
booleans and arrays are JSON. The environment wins over the file. Unknown keys and invalid values
stop startup.

Secrets come from the environment. [`.env.example`](.env.example) lists them with placeholder
values: the database URL, the admin token and the optional provider credentials (Spotify, TIDAL,
Discogs, GitHub, CourtListener, Podcast Index, TMDB, Ticketmaster). Copy it to `.env`, fill in what
you use, and load it (`set -a; . ./.env; set +a`, systemd `EnvironmentFile=`, or
`docker run --env-file`). Keep `.env` out of version control and readable by the service user only.
A source's own variable, such as `PINAKES_GITHUB_TOKEN`, fills its value only when the file and the
`PINAKES__…` form leave it unset. The [API-key guide](docs/api-keys.md) explains each credential and what works without it.

## Test

```sh
# The role needs CREATEDB and permission to create the PostGIS extension.
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
scripts/test
```

`scripts/test` runs `cargo fmt --check`, clippy with warnings denied, every workspace test, the e2e
driver's Node tests (`npm test --prefix e2e`, Node 20 or newer) and `scripts/check-ops`. Integration
tests create their own disposable databases and never touch the development database. HTTP tests use
local servers, injected transports and recorded fixtures. The tests that run
`scripts/verify-osm-boundary` also need `psql`, `pg_dump`, `rg` and `python3`.

End-to-end checks ([e2e/README.md](e2e/README.md)) also need Node 20 or newer:

- `scripts/e2e --mock` runs the whole flow (schema, ingest, queue, resolvers, groups, API) against
  fixtures, offline.
- `scripts/e2e --isolated-live` writes real tags through the Pubky SDK, checks them through a real
  Nexus and public services, and deletes them after.
- `scripts/demo-lookup [--base URL]` resolves a list of public links against a running daemon and
  fails when one misses its expected type, status or derived links. It writes nothing to Pubky or
  Nexus, but `POST /v1/resolve` creates and queues each link in that Pinakes.

## Documentation

| Document | Contents |
| --- | --- |
| [How Pinakes works](docs/design.md) | References, routes, identity, refresh, keys and groups, maybe-same, social content, events, places, what is never indexed |
| [Product decisions](docs/decisions.md) | Each product rule with its reason |
| [API reference](docs/api.md) | The `/v1` API: cards, search, lookups, groups, also-on links, agents and the admin listener |
| [Sources](docs/sources/README.md) | One page per source: links, requests, credentials, cards, keys, credit and switch |
| [Operations](docs/operations.md) | Configuration and limits, database, egress, ingestion, refresh, walls, events, monitoring |
| [Deployment](docs/deployment.md) | Container, CI, what to host, storage, network and an upgrade checklist |
| [API keys](docs/api-keys.md) | Every optional credential, where to get it and what works without it |
| [Pubky apps](docs/pubky-apps.md) | Cards for other Pubky apps' objects, and how to add an app |
| [ops/README.md](ops/README.md) | Metrics, alerts and the Grafana dashboard |
| [data/README.md](data/README.md) | Pinned data sets and their licences |
| [e2e/README.md](e2e/README.md) | The end-to-end harness |
| [AGENTS.md](AGENTS.md) | How to work on the code |

## Repository layout

| Path | Purpose |
| --- | --- |
| `crates/pinakes-core` | The library: ingest, queue, resolvers, sources, joins and the API |
| `crates/pinakesd` | The daemon binary |
| `migrations` | The database schema, `0001_schema.sql` |
| `data` | Pinned data sets: oEmbed providers, event platforms, time zones, wall markers |
| `docs` | Documentation |
| `e2e` | The end-to-end harness |
| `ops` | Alert rules and the Grafana dashboard |
| `scripts` | Development, test and maintenance scripts |

## License

MIT. See [LICENSE](LICENSE).
