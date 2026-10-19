# Pinakes

A Pubky index of external resources: the web pages, places, coordinates, feeds and media that
people reference on Pubky.

Nexus records references and never fetches. Pinakes reads Nexus's reference feed, resolves each
URI into one small typed document through a resolver per kind, keeps the facts a query needs
beside the public references, and answers card reads, lookups, groups and a change feed. The
rule for clients is **Pinakes ranks, Nexus hydrates**: clients pass the URIs of the cards
Pinakes returns (`uri`, and `details.aliases` for other spellings) to Nexus
`/v0/stream/resources/by_ids` as `uris` (batches of 100) for social state. Pinakes IDs are its
own: the two combine by URI, never by ID.

- [docs/design.md](docs/design.md): what Pinakes is and the decisions behind it.
- [docs/operations.md](docs/operations.md): configuration, limits, deployment and maintenance.
- [ops/README.md](ops/README.md): metrics, alerts and the Grafana dashboard.
- [data/README.md](data/README.md): pinned data sets and their licences.

## What it resolves

- **Web pages**: JSON-LD, microformats, OpenGraph, oEmbed and title, with access-wall detection.
- **Pubky objects** of other apps, from conventional JSON members.
- **Places**: `geo:` URIs and map links offline; OpenStreetMap objects through batched Nominatim
  lookups, kept in an ODbL derivative table.
- **Cards**: media (oEmbed, podcasts), ISBN (Open Library), DOI (CSL) and ActivityPub.
- **Aliases and groups**: spellings of one object (tracking parameters, OSM URL forms, ISBN-10)
  are one resource; a DOI or ISBN and the landing page that cites it form a group: a hint on
  each member's card, never a merge.

It also keeps a change journal, so clients can ask what changed for the links they cached.

## Running locally

Requires Rust 1.89 or newer, PostgreSQL **16–18** with PostGIS **3.4+**, `pg_trgm` and h3. Docker
is not required. On Ubuntu, install
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
curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' \
  http://127.0.0.1:8090/v1/resolve
```

`pinakes.example.toml` lists the operator choices: addresses, credentials, one `enabled` switch
per resolver and feature, egress lists and the public API's budget.
Every cap, timeout and refresh tier has a default in an optional `[limits]` section,
documented in [docs/operations.md](docs/operations.md).

Pubky objects are fetched through the same egress policy as web pages, from the homeserver's
public HTTPS domain; see [operations](docs/operations.md#pubky).

## Configuration and secrets

Choices live in the TOML file; secrets and per-deployment overrides live in the environment.
Any key can be set as `PINAKES__SECTION__KEY` (`PINAKES__WEB__ENABLED=false`,
`PINAKES__LIMITS__POLL_SECS=10`): strings are literal, numbers, booleans and arrays use JSON, and
the environment wins over the file. A host rate (`PINAKES__LIMITS__HOST_RATES__<HOST>`) creates
its entry. Unknown keys and invalid values stop startup, so a misspelt variable is an error, not a
silent default.

[`.env.example`](.env.example) lists every variable this version reads for secrets (the database
URL with its password and the admin token) with fake values. Copy it to `.env`,
fill in what you use and load it:

- a shell: `set -a; . ./.env; set +a`, then start `pinakesd`;
- systemd: `EnvironmentFile=/etc/pinakes/pinakes.env` in the unit;
- a container: `docker run --env-file .env …`.

Keep `.env` out of version control (it is in `.gitignore`) and readable by the service user only;
never commit real keys, tokens or passwords.

## Schema

The schema is in `migrations/` (`0001_extensions.sql`, `0002_schema.sql`);
`pinakesd --config <path> --migrate` builds or migrates a database. A parser or routing change
reaches stored documents through `pinakesd --config <path> reextract <route…|all>`.

## Layout

- `crates/pinakes-core`: schema access, the Nexus feed client, resolvers, the queue, grouping,
  the change journal and the API.
- `crates/pinakesd`: the daemon: migrations, the two HTTP listeners, feed polling and workers.
- `data/`: the pinned schema.org vocabulary, oEmbed registry, wall markers and multi-tenant
  host rules.
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

- `scripts/verify-osm-boundary [out.sql]` checks a resolved database keeps OSM-derived facts
  only in `osm_place`, and can export that table.
