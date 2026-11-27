# Deployment

This page covers the services, storage and network around `pinakesd` in production: the
container image, how CI builds and publishes it, what else to host, and what to check after
deployment. Configuration names come from [`pinakes.example.toml`](../pinakes.example.toml).
[API keys](api-keys.md) covers credentials, and [operations](operations.md) covers every setting
and procedure.

## Container

The [Dockerfile](../Dockerfile) builds the release `pinakesd` binary in the `rust:1.98.1-slim-bookworm` image
for the platform being built, then copies it into a `debian:bookworm-slim` runtime image.

| Property | Value |
| --- | --- |
| User | UID/GID `10001:10001` (`pinakes`), no shell |
| Packages | CA certificates and `libgcc-s1` only; no PostgreSQL client and no `curl` |
| Exposed port | `8090` only |
| Working directory | `/var/lib/pinakes` |
| Entry point | `/usr/local/bin/pinakesd` |
| Health check | `pinakesd healthcheck` every 30 seconds (timeout 10 s, start period 60 s, 3 retries) |
| Configuration and credentials | None; you mount them |

The image sets five settings as environment variables:

| Variable | Value |
| --- | --- |
| `PINAKES__LISTEN` | `0.0.0.0:8090` |
| `PINAKES__ADMIN_LISTEN` | `127.0.0.1:9090` |
| `PINAKES__OSM__DUMP_DIR` | `/var/lib/pinakes/dumps` |
| `PINAKES__CATALOGUES__DIR` | `/var/lib/pinakes/catalogues` |
| `PINAKES__SEARCH__INDEX_DIR` | `/var/lib/pinakes/search-index` |

The environment wins over the TOML file, so `listen`, `admin_listen`, `osm.dump_dir`,
`catalogues.dir` and `search.index_dir` in a mounted TOML file have no effect. To change one, pass the variable, such as
`--env PINAKES__LISTEN=…`.

### Build the image

On a machine with Docker and enough build space, build and check the image:

```sh
docker build --pull --tag pinakes:local .
docker run --rm pinakes:local --help
```

### Prepare the configuration

1. Copy `pinakes.example.toml` to a path the container can mount. Set `nexus_url`,
   `osm.nominatim_url` and `[http] contact`, and switch off the sources you don't use.
2. Create the environment file from [`.env.example`](../.env.example), readable only by its owner
   (`chmod 600`). Set `PINAKES__DATABASE_URL`, `PINAKES__ADMIN_TOKEN` and any provider credentials
   there.

The daemon reads `PINAKES__DATABASE_URL`. `DATABASE_URL` alone is a setting of the test tools.

Inside the container, loopback is the container itself. Use names the container can reach for
PostgreSQL, Nexus and Nominatim.

### Run the container

Set `PINAKES_IMAGE` to a published version or digest, or to `pinakes:local` for a local build.
The commands start the daemon with a read-only root file system, a named volume for the ODbL
dump, and the public port published on the host's loopback only:

```sh
PINAKES_IMAGE=ghcr.io/pubky/pinakes:v0.1.0
docker pull "$PINAKES_IMAGE"
docker volume create pinakes-dumps
docker run --detach --name pinakes --restart unless-stopped \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,noexec,nosuid,size=64m \
  --publish 127.0.0.1:8090:8090 \
  --env-file /absolute/path/pinakes.env \
  --mount type=bind,source=/absolute/path/pinakes.toml,target=/etc/pinakes/pinakes.toml,readonly \
  --mount type=volume,source=pinakes-dumps,target=/var/lib/pinakes/dumps \
  "$PINAKES_IMAGE" --config /etc/pinakes/pinakes.toml
curl --fail http://127.0.0.1:8090/v1/info
curl --fail 'http://127.0.0.1:8090/v1/search?limit=1'
```

UID 10001 must be able to read the mounted configuration file. A new named volume takes the
ownership of the image's dump directory. For an existing volume or a bind mount, give UID/GID
`10001:10001` write access before you start the container. Keep the dump volume across
container replacements and back it up. The daemon writes the dump through its own database
connection, so the image needs no PostgreSQL client. Run database backups from a separate
PostgreSQL client that matches the server's major version.

### Health

The image's health check runs `pinakesd healthcheck`, which asks `GET /healthz` on the admin
listener (on loopback inside the container; no token needed). It is healthy when the database
answers `SELECT 1` within a second and no background task has ended. To see the status:

```sh
docker inspect --format '{{.State.Health.Status}}' pinakes
```

### Run commands with the image

Put a TLS proxy in front of the published public port. The admin port 9090 stays on loopback
inside the container: don't publish it, and don't use host networking to reach it. To scrape it
from elsewhere, see [network and secrets](#network-and-secrets).

One daemon owns one database. Stop the old container before you start its replacement.

To run a command, put it after the image name, with the same mounts and environment file:

- `--config /etc/pinakes/pinakes.toml --migrate`
- `--config /etc/pinakes/pinakes.toml reextract all`, with the daemon stopped
- `--config /etc/pinakes/pinakes.toml reindex`, with the daemon stopped
- `--config /etc/pinakes/pinakes.toml catalogue status`, beside the daemon
- `--config /etc/pinakes/pinakes.toml catalogue import <name>`, with the catalogue off in the
  running daemon (or the daemon stopped)

## CI and image publishing

[CI](../.github/workflows/ci.yml) runs on pull requests, on pushes to `main` and on tags that
match `v*.*.*`. Rust is 1.98.1 in CI, in the image build and in `rust-toolchain.toml`.

| Job | What it does |
| --- | --- |
| `Full test gate and mock e2e` | Sets up a disposable PostgreSQL 16 cluster with PostGIS (`scripts/ci/setup-postgres`; it fails below PostGIS 3.4). Runs the workflow contracts (`scripts/ci/test_contracts.py`), then `scripts/ci/check`: actionlint, Hadolint, ShellCheck and a whitespace check of the change against `main`. Then runs `scripts/test` and `scripts/e2e --mock`. |
| `Build and smoke image (amd64)`, `(arm64)` | Builds the image natively on `ubuntu-24.04` and `ubuntu-24.04-arm` without logging in to a registry, and runs `scripts/ci/container-smoke` against a real database. |
| `Publish platform (amd64)`, `(arm64)` | Tags only. Builds, pushes and smoke-tests one platform image ([publishing](#how-a-tag-is-published)). |
| `Publish multiarch manifest` | Tags only. Combines the two tested platform images and sets the release tags. |

`scripts/ci/container-smoke` starts the release daemon and checks that `/v1/info` and
`/v1/search?limit=1` answer, the ODbL dump downloads, the process runs as UID 10001, the health
check passes, the image exposes only port 8090, and port 9090 is bound to loopback only. It needs
no public provider and no test identity.

Pull request and `main` runs of this workflow don't log in to a registry, and their token can't
write packages: only the two publishing jobs ask for `packages: write`. The workflow creates no
Git tags, releases or deployments.

### How a tag is published

Only a pushed, valid `v`-prefixed SemVer tag of at most 128 characters publishes
(`scripts/ci/release-tag`):

| Git tag | Image tags under `ghcr.io/pubky/pinakes` |
| --- | --- |
| `v1.2.3` | `v1.2.3`, the full commit SHA, `latest` |
| `v1.2.3-rc.1` | `v1.2.3-rc.1`, the full commit SHA; `latest` is unchanged |
| `v1.2.3+build.9` | `v1.2.3_build.9`, the full commit SHA, `latest` |

The underscore keeps build metadata apart from a prerelease name. `latest` follows the last
stable tag that was published, not the highest version number. Deploy an immutable version or
digest.

The publishing jobs run after both test jobs pass:

1. Each job checks the tag, then checks that `main` itself pointed to the tagged commit on its
   first-parent line (`scripts/ci/release-on-main`). A tag on a feature branch, or on a commit
   that reached `main` only inside a merged branch, publishes nothing. The check peels the
   pushed SHA to its commit (`git rev-parse "$GITHUB_SHA^{commit}"`) and requires the checked-out
   tree to be that commit. For an annotated tag, such as the signed one below, the commit-named
   image tags still name the commit and never the tag object.
2. Each platform job sets up its database before it logs in to the GitHub Container Registry
   (`ghcr.io`), so no registry credential is on disk during package installs.
3. Each platform job builds its image with the org's
   `build_and_push` action (`pubky/ci-workflows`, pinned to a commit in `.github/workflows/ci.yml`)
   and pushes it as `<full-commit-SHA>-<run-ID>-amd64` or `<full-commit-SHA>-<run-ID>-arm64`. The Dockerfile builds inside each platform's Rust image and ignores the action's `TARGETARCH` build argument.
   The action's `latest` and `sha_tag` inputs are `false`, so platform jobs never write the shared
   tags.
4. Each platform job pulls the image it pushed and runs `scripts/ci/container-smoke` on it: the
   pushed bytes come from a separate build from the one the test job checked.
5. The manifest job (`scripts/ci/publish-manifest`) checks that the combined manifest holds
   `linux/amd64` and `linux/arm64`, then sets the version tag, the full-SHA tag and, for a stable
   version, `latest`. The combined manifest carries the `org.opencontainers.image.source`
   annotation, and each platform image the same label from the `Dockerfile`, so the package links
   to this repository.

The per-platform tags belong to one workflow run. Two tags on the same commit start two runs, and
neither can replace the bytes the other tested. A re-run of a failed job keeps the run ID, so it
replaces and tests its own tag again before the manifest job reads it. A failed smoke test leaves
its run's tag in the registry, never promoted.

The `main` check runs from the tagged commit's own workflow, so it stops mistaken tags, not
someone who can push tags or branches. `GITHUB_TOKEN` is not limited to a ref or an Environment:
anyone with write access to the repository can push a branch whose workflow asks for
`packages: write` and writes to `ghcr.io/pubky/pinakes`, release tags included. Pull requests
from forks get a read-only token. Keep write access to release maintainers; the repository
settings below cover tags and `main`.

### Repository settings for publishing

Owner prerequisites, not set by this repository's files:

- A branch protection rule or ruleset on `main` with these required status checks, without bypass:
  `Full test gate and mock e2e`, `Build and smoke image (amd64)` and
  `Build and smoke image (arm64)`. Then every state on `main`'s first-parent line passed them, as
  `scripts/ci/release-on-main` assumes.
- A tag ruleset for `refs/tags/v*` that lets only release maintainers create, update or delete
  those tags.
- Optionally, a `release` Environment limited to `v*` tags, with required reviewers if you want
  them, as an approval step for the publishing jobs. It holds no credential, so it doesn't stop
  another workflow from writing packages. Create it with its rules first, then add
  `environment: release` to the `publish` and `manifest` jobs: naming an Environment in the
  workflow creates it with no protection rules.
  `scripts/ci/test_contracts.py` refuses an `environment:`, so change that contract in the same
  commit.
- The organization's package settings (Settings, Packages, Package creation) let members create
  public container packages.
- After the first release, the package `ghcr.io/pubky/pinakes` is public (package settings,
  Change visibility). While it is private, `docker pull` without a login fails. An organization
  owner or a package admin can change it.
- No package named `ghcr.io/pubky/pinakes` exists before the first release, or it gives this
  repository write access (package settings, Manage Actions access). A package first pushed by
  hand or with a personal token, even one linked by the image's source label, doesn't give this
  repository's workflows write access, and the first publish fails with 403.

Publishing needs no stored secret. The two publishing jobs log in to `ghcr.io` with the run's own
`GITHUB_TOKEN`, which only they get with `packages: write`. A workflow that creates the package
with that token links it to this repository and gives the repository admin access to it, so later
releases can push new versions.

### Release a version

First update the workspace version, complete review and CI on that commit, and check the
repository settings above. These commands create and push the signed tag; they don't deploy the
image:

```sh
VERSION=v0.1.0
git tag -s "$VERSION" -m "Pinakes $VERSION"
git push origin "refs/tags/$VERSION"
# After the publish jobs succeed, check that the manifest holds both platforms.
docker buildx imagetools inspect "ghcr.io/pubky/pinakes:$VERSION"
```

## What to host

| Service | What Pinakes uses it for | Requirements |
| --- | --- | --- |
| `pinakesd` | The public API and a separate private admin listener | One daemon per database; the instance lock enforces it. Request budgets are per process: several instances behind one address share the services' limits. Load-test your own mix of links for sizing. |
| PostgreSQL 16 to 18 | Documents, references, queues, groups and the change journal | PostGIS 3.4 or newer and `pg_trgm`. Size it from the [storage model](#storage-model). |
| Nominatim | Venue geocoding and OpenStreetMap place details (`osm.nominatim_url`) | Self-hosted or hosted ([Nominatim](#nominatim)). For hardware, see [Nominatim's installation guide](https://nominatim.org/release-docs/5.2/admin/Installation/). |
| Nexus | The reference feed and social and reach reads (`nexus_url`) | A Nexus that serves the `/v1` routes Pinakes reads ([README](../README.md)). It is a fixed, trusted upstream; optional authorization comes from the environment. |
| Dump directory | `osm.dump_dir`, served at `/v1/dumps/osm_place.ndjson.gz` | Persistent and writable, with room for two copies of the file ([ODbL dump](operations.md#odbl-dump)). A CDN can cache the file by its `ETag`. |
| Catalogue directory | `catalogues.dir`: downloads and temporary files of catalogue imports, one directory per catalogue | Writable by UID 10001, with room for the largest large catalogue's full import ([catalogues](#catalogues)). It need not be backed up. |
| Text index directory | `search.index_dir`: the index text searches read ([text index](operations.md#text-index)) | Local disk, writable by UID 10001, with room for two copies during `reindex` (about 0.7 KB per searchable unit on the development data, and 170 B per catalogue card measured on 100,000 synthetic cards). Derived data: a missing index is rebuilt from the database at start, so a volume only saves that rebuild. |
| Prometheus and Alertmanager | Scraping the admin `/metrics` and evaluating the supplied alerts | A private scrape path. [ops/README.md](../ops/README.md) has the rules and a Grafana dashboard. |
| MusicBrainz mirror (optional) | Music lookups, at `[also_on.sources.musicbrainz] base_url` | Optional. The mirror's host gets the shared per-host pace, and a mirror on a private address is refused ([operations](operations.md#also-on)). See the [MusicBrainz Server documentation](https://musicbrainz.org/doc/MusicBrainz_Server). |

Pinakes calls no place search, autocomplete or map tile service. Clients that need them, such as
[Photon](https://github.com/komoot/photon) for place type-ahead or
[Protomaps PMTiles](https://docs.protomaps.com/deploy/) for map tiles, call them directly.

## Nominatim

Pinakes geocodes venues and reads OpenStreetMap place details from one Nominatim, at
`osm.nominatim_url`. There is no other geocoding provider and no failover. Run one of these:

- **A self-hosted Nominatim.** Its URL carries no secret and can stay in the TOML file.
- **A hosted Nominatim,** such as [Geofabrik's](https://geofabrik.de/data/geocoding.html). Its
  URL may carry the account's key, so treat the whole URL as a secret: set it as
  `PINAKES__OSM__NOMINATIM_URL` in the environment file, never in the TOML file. The key must be
  part of the URL's path; a URL with user info, a query or a fragment is refused at startup.
  Pinakes logs a request by its path below the base alone (`lookup`, `search`), shows the URL as
  `[REDACTED]` in debug output and serialization, and never shows it in `/v1/info`. Set
  `osm.requests_per_second` to the plan's allowance.

The default, the public `nominatim.openstreetmap.org` at 0.5 requests per second, is for a small
development run only ([operations](operations.md#nominatim-and-nexus)).

## Storage model

Each environment keeps its own database, admin token and secrets. Staging and production can
share read-only self-hosted services, such as Nominatim or a MusicBrainz mirror. Budget staging
traffic so that it cannot use up production's share of a provider's limits or a shared service's
capacity.

### Measured baseline

On 2026-11-02, the public production Nexus graph had **225 `Resource` targets with 1,198 incoming
`TAGGED` edges**. Tags on `Post` or `User` targets are not tags on external links. These counts
are a sizing input; a given Pinakes database does not hold the same rows as the graph. Replies,
embeds, collections, app resources and previews also add to storage.

A running instance, measured separately, had:

| Measure | Value |
| --- | --- |
| Rows in `resource` | 1,725 |
| Rows in `reference` | 6,221 |
| Database size (`pg_database_size`) | 65,133,591 bytes |
| User tables and their indexes, without PostGIS's fixed `spatial_ref_sys` | 47,046,656 bytes |
| Custom-format `pg_dump --no-owner --no-acl` | 2,458,465 bytes |

This sample includes queues, groups, occurrences, the change journal, the OSM cache and indexes.
It is not a per-card JSON size.

### Estimate

The table below is an estimate in decimal units. It assumes:

- 30 KB per resource, references included, at the measured mix, plus 20 MB fixed overhead;
- tag counts scale with the graph snapshot and are not charged a second time;
- provisioning of 1.5 times the estimated database size, plus a 3 GB floor for WAL;
- compressed backups of 10–30% of the database size, above the sample's compression.

| Scale | External links | Tags on those links | Estimated database | Data and WAL to provision | Compressed backup |
| --- | --- | --- | --- | --- | --- |
| Baseline | 225 | 1,198 | ~27 MB | ~3 GB | ~3–8 MB |
| 100× | 22,500 | 119,800 | ~695 MB | ~4 GB | ~70–210 MB |
| 100,000× | 22.5 million | 119.8 million | ~675 GB | ~1.02 TB | ~70–210 GB |

Longer change journals, larger documents, resources with many references, table bloat,
replication slots and WAL retention can dominate. At large scale, measure them instead of
extrapolating from this sample.

### Measure your own instance

These queries show no credentials:

```sql
SELECT pg_database_size(current_database());
SELECT count(*) FROM resource;
SELECT count(*) FROM reference;
SELECT sum(pg_total_relation_size(relid))
FROM pg_stat_user_tables WHERE relname <> 'spatial_ref_sys';
```

To count the graph again, count `MATCH ()-[t:TAGGED]->(x:Resource)` and return `count(t)` and
`count(DISTINCT x)`. Don't count every tag in the network as a reference to an external resource.

### Catalogue cards

A catalogue card takes about 1 KB in PostgreSQL with its indexes (measured on 1.9 million cards).
Every catalogue is on by default; switch off the ones you don't want
([operations](operations.md#catalogue-sections)). A small instance takes a part:
`catalogues.osm.region` for one region of OpenStreetMap, `catalogues.wikidata.limit` and
`catalogues.musicbrainz.limit` for the first items of those.

### Backups

Back up off-host and test your restores. Dump the whole database, not only `resource`: operator
curation (`group_override`) and the change journal (`resource_change`) cannot be rebuilt from a
Nexus replay. Budget backup retention apart from the database itself.

## Catalogues

Each catalogue's importer follows these ([importer rules](operations.md#importer-rules)):

- **Free disk.** Before a run starts or resumes, its importer compares the free disk under its
  directory (`catalogues.dir/<name>`) with what the run still needs. While it is short, the run
  waits, and `pinakesd catalogue status` and the log say why. Only that file system is checked;
  size PostgreSQL's disk from the [storage model](#storage-model).
- **One large import at a time.** Full imports of Wikidata, MusicBrainz, OpenStreetMap and
  OpenAlex run one at a time, so their temporary files don't pile up: one cut off by a failure or
  a restart keeps the turn until it has finished. Give `catalogues.dir` room for the largest of
  them. A large catalogue switched off in the middle of its full import keeps its files there
  until it is on again and finishes, or until `pinakesd catalogue drop <name>` deletes its
  directory with its cards.

### MusicBrainz

The importer needs `http.contact` ([`musicbrainz` catalogue](sources/musicbrainz_catalogue.md)).
Measured on a sample of the real dump (the first 60 MB of the release file and a few MB of the
others: 63,313 lines, 177,190 cards) on a 20-core host, and estimated from it for the whole
dump (48.8 million cards):

| | Sample | Whole dump (estimate) |
| --- | --- | --- |
| Download | 84 MB a dump | 26 GB, once per run, streamed (MusicBrainz serves at about 35 MB/s) |
| Disk under `catalogues.dir/musicbrainz` | 51 MB of sorted runs (0.6 of the files) | about 16 GB for an import, 19 GB for an update (the stored cards' hashes too); a run waits for 40 GB free (`DISK_NEED`), less what a cut-off run left there |
| PostgreSQL | 254 MB (1.4 KB a card with its indexes) | about 70 GB |
| Memory | 195 MB for the import, 214 MB for the update | about 1.5 GB (512 MiB of records being sorted, the xz decoder, the lines in flight) |
| Read and sort | 2.2 s | 15–20 minutes, bound by the download |
| Write | 8.1 s (21,800 cards a second) | about 40 minutes for a full import; an update writes only what changed (0.9% of the sample's cards from one dump to the next) |

A full import takes about an hour; the text index then reads the new cards in the background
([text index](operations.md#text-index)).
### OpenStreetMap

The `osm` catalogue ([source page](sources/osm_catalogue.md)) starts once `http.contact` is set.
With the default `catalogues.osm.region = "planet"`, a daemon with the catalogue on downloads the
planet file as soon as `catalogues.dir` has the room: set a region (`europe/switzerland`) on a
small or staging instance, or switch the catalogue off.

| | Planet | A region |
| --- | --- | --- |
| Download | About 96 GB (`planet-latest.osm.pbf`: 96,072,105,092 bytes on 2026-11-26) | The extract (Switzerland about 550 MB) |
| Temporary disk while importing | The file, the node index (about 115 GB, dense; the import stops with `disk` past about 172 GB) and the set-aside relations (a few GB) | The file and about twice its size (the node index, 16 bytes per node) |
| Memory | Up to about 2.5 GB for the relations' member ways, plus the decoded blocks waiting: at most 2 per decoding thread (at most 8 threads), each at most about 250 MB (real blocks: a few MB), so at most about 4 GB | Hundreds of MB |
| Cards | About 35 million (estimate), about 1.4 KB each in PostgreSQL with their indexes: about 49 GB. The map cells' totals add a few million rows (Switzerland: 48,914 rows, 7.5 MB) | Switzerland: 400,838 cards, 1,391 B each (measured) |
| Import time | About 15 hours plus the download at the measured rate (673 cards/s, a debug build on a busy host; a release build is faster). The map cells' triggers take about 40% of a card write (Switzerland: 100,000 located cards in 7.7 s with them, 4.7 s without) | Switzerland: 596 s after a 552 MB download (measured, debug build) |
| Updates | One daily change file (about 130 MB compressed), read to `catalogues.dir/osm`; about 1 GB of memory while it is applied (estimate: every object's last version, the changed nodes' locations); a file that would hold more than 1 GiB is refused (`too_large`) | The region's daily file (kilobytes to a few MB) |
| Update lag | Updates wait while a full import runs (every 90 days, about 15 hours plus the download, estimate): the lag can pass the 2-day alert when the import waits for the large-import turn or free disk | Minutes of import every 30 days |
| ODbL dump | Adds one line per imported place, written from one repeatable-read snapshot: Switzerland's 400,839 places took 32 s and 21 MB compressed (measured), so the planet's about 45 minutes and 2 GB (estimate); the dump directory holds two copies while one is written | Megabytes |

The file and the index are removed once the import has finished; every 30 days (the planet: every
90) a full import reads a new file again.

### Wikidata

The importer needs `http.contact` ([the Wikidata catalogue](sources/catalogue_wikidata.md)).
Catalogues are on by default: a daemon whose configuration has no `[catalogues]` section and sets
`http.contact` starts the Wikidata importer, which waits for 129 GB free under `catalogues.dir`,
downloads the 104 GB dump and puts its cards in search. An instance that should import nothing sets
`[catalogues] enabled = false`; one that should import other catalogues but not Wikidata sets
`[catalogues.wikidata] enabled = false` (or `limit = N` for a part of it, read as a stream).
Measured on a 150,000-card sample of the real dump; full-size figures are estimates:

| Catalogue | Disk under `catalogues.dir` while it runs | PostgreSQL | Memory | First import | Updates |
| --- | --- | --- | --- | --- | --- |
| `wikidata` | The dump, about 104 GB, until the import ends (and the label files, kept). A first import waits for 129 GB free before it knows the dump's size, then for the dump's size and 4%, and a resumed download for what is left of it and 4 GB | About 21.4 million cards, estimated at 35-45 GB with their indexes (3.3 KB a card on the first 150,000 items, which are the best known and carry the most keys) | Up to about 1 GB for the read (bounded queues; decoded text waiting to be cut is held near 512 MB), plus the labels of classes and genres, which the daemon keeps in memory between updates (estimate: a few million labels, a few hundred MB; at most 16 million), and during an update one API answer at a time (up to 64 MB of JSON, a few hundred MB once parsed) | About 3 hours to download on two connections at Wikimedia's ~5 MB/s each, then the read on half the cores (estimate) | Every 15 minutes, one request at a time: the changed items' sitelinks first, and only those with a Wikipedia article (or a label the catalogue keeps) read in full. Measured on one 19-minute window of real changes (a 20,000-card sample with room to grow): 2,179 items named in 90 s, about 24 a second. Estimate at full size: a median day of changes (about 240,000 items) in about 3 hours, the 2-3 days a first import starts behind in 6-8 hours |

With `catalogues.wikidata.limit` the dump is read as a stream and nothing is downloaded to disk;
the stream is read only up to the limit.

### OpenAlex, Podcast Index, software and Tranco

What each importer needs (measured on samples, see each catalogue's page; the full numbers are
estimates):

| Catalogue | Free disk it waits for (`catalogues.dir`) | Memory while it reads | Read | Cards, database |
| --- | --- | --- | --- | --- |
| `openalex` | 32 GB for a full import, 4 GB for an update (nothing is downloaded to disk; the cards' database share) | Up to 16 files' kept cards (50,000 cards each at most: 1-2 GB at worst, a few hundred MB as a rule) and the IDs with a card (64-bit numbers, about 250-500 MB at full size) | 675 GB from S3 in each full import; a new snapshot's newer files after | ~15M cards, ~26 GB |
| `podcastindex` | 20 GB (the unpacked database, 5 GB today, while a run reads it, and the cards) | Under 1 GB | 1.9 GB a week | ~4.8M cards, ~8 GB (1.7 KB a card measured) |
| `software` | 1 GB (streamed) | The crates of the dump, without READMEs, and their downloads, categories and keywords: at most 1 GiB as counted (about 150 MB today) | ~0.6 GB a day (the part of the crates.io dump read, F-Droid's 75 MB index, Flathub's 11 MB) | ~0.4M cards, ~0.4 GB |
| `tranco` | 1 GB | Up to about 250 MB while a list loads (195-219 MB measured); 16 reads after, and the robots.txt rules kept for deferred pages (about 33 MiB at most: 2,000 origins, 16 KiB each counted with every rule's own memory) | The 22 MB list a day; then each homepage and robots.txt once per 90 days (about 140 GB in all at first, estimate) | ~0.65M cards, ~1.1 GB (1.65 KB a card measured) |

## Network and secrets

**Listeners.** Terminate TLS at a reverse proxy in front of the public listener (`listen`, port
8090). Keep the admin listener (`admin_listen`, `127.0.0.1:9090` by default) private:

- A non-loopback `admin_listen` requires `admin_token`; on loopback the daemon starts without one
  and logs a warning.
- The admin listener answers only a request whose `Host` is `localhost`, an IP address or a name
  in `admin_hosts`, and answers 421 to any other. A scraper that reaches it by name (another
  container's `pinakes:9090`) needs that name, as in `admin_hosts = ["pinakes"]`.
- Don't forward the public listener to the admin listener.

**Timeouts.** A request that takes longer than `limits.request_timeout_secs` (30 by default) is
answered 504. The listeners set no limit on connections, idle time or how slowly a request's
headers arrive: set those at the proxy.

**Client addresses.** Behind the proxy, list its address as the daemon sees it in
`api.trusted_proxies` (for the container above, the Docker bridge gateway, such as
`["172.17.0.1"]`). Have the proxy append the client's address to `X-Forwarded-For` (in nginx:
`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`). Then the per-address budgets,
dump downloads and each client's share of new previews (`limits.preview_new_per_client_per_hour`,
100 by default) are each client's own. With the list empty (the default), the header is never
read and every client behind the proxy shares the proxy's budget
([clients and budgets](operations.md#clients-and-budgets)).

**CORS.** Public routes allow every origin by default (`api.cors_allow_origins = ["*"]`), without
cookies or bearer credentials. Set exact origins, or `[]`, where that fits. Admin routes send no
CORS headers.

**Egress.** Allow outbound public HTTP(S) and the PKARR relays in `pubky.pkarr_relays`. Also deny
private, loopback, link-local and metadata addresses at the network layer. Pinakes checks and
pins DNS answers and checks redirects itself for every request a link can steer
([egress policy](operations.md#egress-policy)).

**Bulk files.** The OpenStreetMap catalogue reads its files and change files only from
`download.geofabrik.de`, `planet.openstreetmap.org` and the two buckets the planet's site redirects
them to (`osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com`,
`osm-planet-us-west-2.s3.dualstack.us-west-2.amazonaws.com`), over https, following redirects only
to those names. Allow them where outbound traffic is filtered by host.

**Nexus and Nominatim.** These are trusted upstreams the operator configures, outside that
policy: their clients follow no redirect and don't apply the DNS pinning or the private-address
refusal. Protect their configuration and DNS. Their URLs cannot hold user info, a query or a
fragment. A protected Nexus gets `PINAKES_NEXUS_AUTHORIZATION`, never a URL with credentials.

**Secrets.** Set an operator contact (`[http] contact`), inject secrets through a protected
environment file, and keep tokens out of logs. Set up only the providers you use
([API keys](api-keys.md)).

## Day-two checklist

1. **Before an upgrade,** take a backup and test a restore. The daemon refuses a database built
   from another version of the schema file. Follow the upgrade procedure in
   [schema](operations.md#schema), and never recreate a production database as a shortcut.
2. **Before re-extraction,** stop the daemon, run `pinakesd --config <path> reextract <route…|all|also-on>`,
   then start it again. [Re-extraction](operations.md#re-extraction) lists the routes and queues
   each one affects.
3. **After a change,** check public `/v1/info`, a few representative cards and searches, the
   private admin info and queue progress. Process health alone doesn't show that ingestion or
   credentialed enrichment works.
4. **Monitoring:** scrape the private `/metrics` and load the [supplied rules](../ops/README.md).
   Alert on stalled ingestion, dead letters, queue backlog and canary failures. Canaries are on by
   default. Every `limits.canary_interval_secs` (6 hours) they send real requests to event platforms
   and read the web, OSM (Nominatim), media, ISBN and DOI samples. `[canary] enabled = false` stops
   them ([canaries](operations.md#canaries)).
5. **Attribution and caches:** keep OpenStreetMap attribution and the dump available. Don't put a
   cache in front of social excerpts that would keep them after Pinakes refreshes or purges them.
   Each source's credit and limits are on its page in the [source catalogue](sources/README.md).
