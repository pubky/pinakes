# Working on Pinakes

Notes for developers and coding agents. [README.md](README.md) explains what Pinakes is and how
the pipeline fits together; read it first.

## Build and test

All commands run from the repository root. Tests that touch the database need a PostgreSQL
server with PostGIS and `pg_trgm` (see the README) and a role that can create databases:

```sh
export DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql'
```

| Command | What it checks |
| --- | --- |
| `scripts/test` | The full gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, the e2e driver's Node tests (`npm test --prefix e2e`), `scripts/check-ops`. Run it before you commit. |
| `cargo test -p pinakes-core --lib` | Unit tests only. A few need `DATABASE_URL` and fail without it. |
| `cargo test -p pinakes-core --test <name>` | One integration test file in `crates/pinakes-core/tests/` (`api`, `groups`, `also_on`, `tidal`, …). Each test gets a fresh database with the schema (`#[sqlx::test(migrations = "../../migrations")]`). |
| `scripts/check-ops` | `promtool check rules ops/alerts.yml` when promtool is installed; otherwise the test that checks every metric in `ops/alerts.yml` and `ops/grafana/pinakes.json` against the registry (`telemetry.rs`), which `cargo test` also runs. |
| `npm ci --prefix e2e && npm test --prefix e2e` | The e2e harness's own unit tests. |
| `scripts/e2e --mock` | The whole flow against fixtures, offline: migrations, ingest, queue, resolvers, groups, API. First, that a cancelled `--live-recipes` run drops its database and still reports (`e2e/cancel.test.mjs`, which lets egress reach no host). |
| `scripts/e2e --isolated-live` | The same against a real homeserver, Nexus and public services. Needs a test identity ([e2e/README.md](e2e/README.md)). Then runs `--live-recipes`. |
| `scripts/e2e --live-recipes` | Resolves each public `/v1/resolve` example (`data/api-recipes.json`, the sources') live, once, and checks its card's type; a recipe this host could not check (host out of reach, a bot wall, rate limited) is left out with the reason. Needs no identity. |
| `scripts/demo-lookup [--base URL]` | Resolves the curated links in `e2e/samples.lookup.json` against a running daemon. Writes nothing to Pubky or Nexus; creates and queues the links in that Pinakes (`POST /v1/resolve`). |
| `scripts/openapi` | Regenerates `docs/openapi.json`. A test fails while the snapshot differs from the code. |
| `scripts/verify-osm-boundary [out.sql]` | OSM-derived facts are only in `osm_place` (run by the tests; needs `psql`, `pg_dump`, `rg`). |
| `scripts/verify-no-capability-urls` | No private calendar share URL in any tracked file (run by the tests). |
| `scripts/check-removals [--tests] [NAME…]` | Removes each source (every registered one, or the named ones) on a scratch worktree by the checklist below, checks nothing of the source's is left (its id, display name and page links, searched independently of the deletion), and builds every test target with warnings as errors; with `--tests`, also runs the workspace's tests; `--self-test` checks the leftover check. A source whose removal needs any other edit fails. Half a minute per source to build, minutes more with the tests; run it for the sources a change touches. |
| `scripts/lock-waits COMMAND…` | Samples the database every 50 ms while COMMAND runs (COMMAND starts once the sampler has its first sample, or has stopped) and prints which locks backends waited on; fails, without repeating any part of `DATABASE_URL`, when it could not sample. Needs `psql` and `python3`. |

Tests use local servers, injected transports and recorded fixtures instead of public services.

## Layout

- `crates/pinakes-core/src/`: everything but the daemon binary (`lib.rs` lists the modules).
  - `ingest.rs`, `nexus.rs`, `collections.rs`: the Nexus reference feed, reach lists and the
    public collections that hold a link.
  - `identity.rs`, `alias.rs`, `capability.rs`: URI normalization, resource IDs, aliases, and
    links that carry a credential.
  - `model.rs`: the stored document (`ResourceMeta`) and the schema.org type table.
  - `queue.rs`, `scheduler.rs`, `store.rs`, `changes.rs`: the resolution queue, refresh times,
    saving documents, the change journal and orphan cleanup.
  - `resolver/`: routing (`classify`), the egress client (`egress.rs`), and the web, pubky, geo,
    OSM and card resolvers. `resolver/pubky_apps/` holds one profile per Pubky app (what its
    objects' cards hold and which clients open them); [docs/pubky-apps.md](docs/pubky-apps.md)
    says how to add one.
  - `event/`: event pages, calendar feeds, event platforms (`adapters/`).
  - `sources/`: official-API sources, one module per service (`kit.rs` holds shared helpers).
  - `kinds/`: kinds of card with facts of their own whichever route reads them (job postings),
    their JSON-LD readers and their `FACTS`.
  - `also_on.rs` and `also_on/` (one module per also-on source), `lookups/` (one module per
    lookup family: places, papers, packages, games, archived copies), and the code they share:
    `hubs.rs` (MusicBrainz, Wikidata), `music.rs`, `music_sources.rs`, `music_identity.rs`,
    `music_enrichment.rs`, `wikipedia.rs`, `wikimedia.rs` (Commons files), `episodes.rs` (a
    show's latest episodes), `podcast_index_api.rs`, `app_ids.rs` and `barcode.rs` (ID forms
    several modules read), `embed.rs` (a card's `embed`).
  - `groups.rs`, `group_keys.rs`, `same_event.rs`, `same_recording.rs`, `hints.rs`,
    `matching.rs`: groups, the same-event and same-recording hints and what they share.
  - `api.rs`, `change_api.rs`, `spatial.rs`, `recipes.rs`: the HTTP API, its OpenAPI document
    and examples. `agents/`: the agents' guide (`/llms.txt`, `/skill.md`) and the MCP endpoint.
  - `config.rs`: configuration and every limit with its default.
  - `telemetry.rs`, `canary.rs`, `maintenance.rs`, `osm_dump.rs`, `locks.rs`, `adapter.rs`
    (each route's manifest: mode, cadence, expected fields, canary), `source.rs` (each source's
    last outcome and its error classes).
  - `bin/pinakes-parse.rs`: the parse worker as a program of its own, for the tests.
- `crates/pinakes-core/tests/`: integration tests; `fixtures/` holds recorded responses, each
  directory with a README or `provenance.json` saying where they came from.
- `crates/pinakes-core/examples/`: `e2e_stack` (the e2e harness's daemon) and `adapter_probe`
  (a read-only canary probe).
- `crates/pinakesd/`: the daemon binary.
- `migrations/0001_schema.sql`: the schema.
- `data/`: pinned data sets (schema.org, oEmbed registry, event adapters, wall markers, tenant
  rules, time zones, API examples). [data/README.md](data/README.md) names each one's source,
  and the checksum of each pinned upstream file.
- `docs/`, `ops/`, `e2e/`, `scripts/`.

## Conventions

**Schema.** There is one file, `migrations/0001_schema.sql`. Until the first deployment a schema
change edits that file, and local and test databases are recreated (the daemon refuses a database
built from another version of the file; tests create their own). Numbered migrations come back
with the first deployment.

**Re-extraction.** Stored documents do not change when a parser does. After you change a
resolver, a routing rule or an alias rule, say in the PR which `pinakesd reextract <route…>` an
operator runs. Also-on results refresh with `reextract also-on`; `reextract all` does both.
When a change adds something operators must do, document it in `docs/operations.md`.

**Keys and facts.** Only a hub (MusicBrainz, Wikidata) or a service's own record of its own ID
makes a join key. What a page says about itself (`sameAs`, a canonical link to another page, an
ISRC in its markup) is a fact at most and never joins two resources, and so is another service's
ID a page names for something it lists (an event page's performer keys an artist only on the
service whose artist page it names). Joins never come from a title or name match.

**Egress.** Every request a reference can steer goes through `resolver::egress::Egress`: never
build another HTTP client for them. The egress pins DNS, refuses private addresses, caps bodies,
paces each host (one request at a time, at `limits.host_requests_per_second` or the service's
published rate) and caches answers. A source with a published limit lowers its host's rate; it
never raises it.

**Secrets.** Keys, tokens and passwords come only from the config file or the environment
(`PINAKES__…`, or a variable a source names for itself, `PINAKES_NAME`: an own section's
`OwnSection::VARIABLES`, an official source's `token_variable`). They never appear in logs, errors, `Debug` output or `/v1/info`. Fixtures
and samples hold no real keys and no private calendar URLs.

**Limits.** A new cap, timeout or rate is a limit in `config.rs` with a default, and gets a row in
the table in `docs/operations.md` (a test checks every limit is listed).

**Sources.** A new official-API source is one module in `sources/`, listed in `REGISTRY`, with
`docs/sources/<name>.md`, a line in `docs/sources/README.md`, `[sources.<name>]` in
`pinakes.example.toml` (alphabetical), recorded fixtures in
`tests/fixtures/sources/<name>/` with `provenance.json`, and examples (`Source::examples`).
Tests check the docs, the example config and the examples against the registry.

**Removing a source** (official, also-on, or a lookup family) is deleting its own files and its
registry lines, nothing else ([docs/sources](docs/sources/README.md#removing-a-source) has the
full list):
- its module: `sources/<name>.rs`, `also_on/<id>.rs` or `lookups/<family>.rs`;
- its `pub mod` line and its entry in `sources::REGISTRY`, `also_on::registry()` or
  `lookups::FAMILIES`;
- `docs/sources/<name>.md` and its line in `docs/sources/README.md`;
- its block in `pinakes.example.toml` (and its variables in `.env.example`);
- its fixture directories (those named after it) and its own test file, if any;
- `e2e/samples/<name>.json`.

Shared code stays. A test that needs a source returns early without it, so no test changes. A
removed source's keys stay in the database until they expire, and keep their grouping rule
(`single_object_namespace` remembers it). If removing a source would need any other edit, first
move what it needs into its own files or into shared code; `scripts/check-removals --tests <name>` proves it. Recordings several modules
read are in a fixtures `shared/` directory, never one named after a source.

**API.** Response types and routes carry `utoipa` annotations; their doc comments become the
OpenAPI text. Run `scripts/openapi` after changing them. Every example in
`data/api-recipes.json` is listed in `docs/api.md` (or its source's page), and a test checks it.

**Docs and comments** describe the code as it is and how to use and run it. No history, no
review notes, no "previously" or "fixed in": that belongs in commit messages and PRs. A reason
for a design is welcome when it explains the code.

**Wording.** Product text (API descriptions, Swagger examples, error messages, docs) is plain and
short: concrete words, short sentences, no filler adjectives, no marketing words, no emoji.
