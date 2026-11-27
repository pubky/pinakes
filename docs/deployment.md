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
| Text index directory | `search.index_dir`: the index text searches read ([text index](operations.md#text-index)) | Local disk, writable by UID 10001, with room for two copies during `reindex` (about 0.7 KB per searchable unit on the development data, and 360-380 B per catalogue card on the real cards of the [small regional profile](#small-regional-profile)). Derived data: a missing index is rebuilt from the database at start, so a volume only saves that rebuild. |
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

A catalogue card takes 1.2-4 KB in PostgreSQL with its indexes, depending on the catalogue (2.1 KB
on average over the [small regional profile](#small-regional-profile)'s 1.3 million cards).
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
  a restart keeps the turn until it has finished. Updates and the other catalogues' runs take no
  turn and run beside them. Give `catalogues.dir` room for the largest large import and the runs
  beside it. A large catalogue switched off in the middle of its full import keeps its files there
  until it is on again and finishes, or until `pinakesd catalogue drop <name>` deletes its
  directory with its cards.
- **A contact.** Every importer needs `http.contact`; without it, it doesn't start and
  `catalogue status` says it waits for one.

### Sizes

Each catalogue at full size. The measured figures come from samples of the real files (each
catalogue's page, and the [small regional profile](#small-regional-profile)); the full-size
figures are estimates from them.

| Catalogue | Cards | A card in PostgreSQL, with its indexes | Database | First import | Downloads |
| --- | --- | --- | --- | --- | --- |
| `wikidata` | 21.4 million (estimate) | 1.6-2.1 KB on average (estimate: an average item has 4-5 keys); the first 100,000-150,000 items, the best known, measured 3.3-3.9 KB with 11-13 keys | 35-45 GB (estimate) | 4-5 hours (estimate): 3 hours to download on two connections at Wikimedia's ~5 MB/s each, the read on half the cores, and about an hour to write again the ~60% of cards whose labels come later in the dump | The 104 GB dump once; then API requests |
| `musicbrainz` | 48.8 million (estimate) | 1.4-1.8 KB (measured: 1.43 KB on a 177,190-card sample of the whole dump, 1.77 KB on a 84,621-card limited subset) | 70-90 GB (estimate) | About an hour (estimate): 15-20 minutes to read and sort, bound by the download (about 35 MB/s), about 40 minutes to write | 26 GB a dump, streamed; two dumps a week |
| `osm` | About 35 million for the planet (estimate); Switzerland 400,838 (measured) | 1.4 KB (1,391 B measured on Switzerland) | About 49 GB for the planet (estimate), and a few million rows of map-cell totals (Switzerland: 48,914 rows, 7.5 MB); Switzerland 0.56 GB | Up to 15 hours plus the download for the planet (estimate from a debug build's 673 cards a second; a release build wrote 5,420 a second on Luxembourg); Switzerland 596 s after the download (measured, debug build) | The planet's 96 GB file every 90 days (a region's extract every 30), and one change file a day (about 130 MB for the planet) |
| `openalex` | About 15 million (estimate) | 1.9-2.1 KB (1,855 and 2,132 B measured on samples) | 28-32 GB (estimate) | About 2 hours (estimate), bound by the read from S3 | 675 GB from S3 for each full import (the first new snapshot 180 days after the last); a new snapshot's newer files after |
| `podcastindex` | About 4.8 million (estimate) | 1.7-2.2 KB (1,678 and 2,143 B measured on samples) | 8-10.5 GB (estimate) | 15-30 minutes (estimate): the 1.9 GB download, then 3,900-10,700 cards a second (measured) | 1.9 GB a week |
| `software` | About 400,000 (396,252 measured on the full files) | 1.2-1.5 KB (1,241 and 1,486 B measured) | About 0.5 GB (estimate) | About a minute (measured: 50.5 s on the full files) | About 0.6 GB a day (the part of the crates.io dump read, F-Droid's 75 MB index, Flathub's 11 MB) |
| `tranco` | About 650,000 (estimate) | 1.65 KB (measured) | About 1.1 GB (estimate), and 0.15 GB for the list's million sites (measured) | The list in seconds; the homepages in about 46 hours (estimate: about 6 a second, 16 at a time), after the large imports | The 22 MB list a day; each homepage and robots.txt once per 90 days (about 140 GB in all at first, estimate) |
| All | About 126 million (estimate) | | 190-230 GB (estimate) | 15-25 hours of large imports, one at a time (estimate) | |

The text index takes 360-380 B a catalogue card (measured on 1.3 million real cards): about 48 GB
for all of them (estimate).

### Disk and memory while importing

| Catalogue | Free disk it waits for under `catalogues.dir` | Disk while it runs | Memory |
| --- | --- | --- | --- |
| `wikidata` | 129 GB before it knows the dump's size, then the dump's size and 4 GB; a resumed download what is left of it and 4 GB; 1 GB with a `limit` and for an update | The dump until the import ends, and the label files (kept); nothing with a `limit` (read as a stream up to the limit) | Up to about 1 GB for the read (bounded queues; decoded text waiting to be cut is held near 512 MB), plus the labels of classes and genres, kept between updates (estimate: a few hundred MB; at most 16 million labels), and during an update one API answer at a time (up to 64 MB of JSON, a few hundred MB once parsed) |
| `musicbrainz` | 40 GB (`DISK_NEED`), less what a cut-off run left; with a `limit`, 16 KiB a card of it | Sorted runs: 0.6 of the dump files' bytes on a sample, so about 16 GB for an import and 19 GB for an update (estimate; the stored cards' hashes too) | About 1.5 GB (estimate: 512 MiB of records being sorted, the xz decoder, the lines in flight); 158-229 MB measured on samples |
| `osm` | The planet 215 GB (120 GB to resume); a region 2 GB, then three times its file once it is downloaded; an update 1 GB | The file, the node index (the planet's about 115 GB, dense: the import stops with `disk` past about 172 GB; a region's about twice its file, 16 bytes a node) and the set-aside relations (a few GB); removed once the import has finished | The planet up to about 4 GB: 2.5 GB for the relations' member ways, plus decoded blocks waiting (at most 2 per decoding thread, at most 8 threads, each at most about 250 MB; real blocks are a few MB); a region hundreds of MB (131-225 MB measured); an update about 1 GB (estimate), and a change file that would hold more than 1 GiB is refused (`too_large`) |
| `openalex` | 32 GiB for a full import, 4 GiB for an update (the cards' database share; nothing is downloaded to disk) | Nothing | Up to 16 files' kept cards (50,000 cards each at most: 1-2 GB at worst, a few hundred MB as a rule) and the IDs with a card (64-bit numbers, about 250-500 MB at full size) |
| `podcastindex` | 20 GiB, less what a cut-off run's unpacked database holds | The unpacked database (5 GB today) while a run reads it | Under 1 GB (54-83 MB measured) |
| `software` | 1 GiB | Nothing (streamed) | The crates of the dump, without READMEs, and their downloads, categories and keywords: at most 1 GiB as counted (about 150 MB today) |
| `tranco` | 1 GiB | Nothing | Up to about 250 MB while a list loads (195-219 MB measured); 16 reads after, and the robots.txt rules kept for deferred pages (about 33 MiB at most: 2,000 origins, 16 KiB each counted with every rule's own memory) |

### Notes per catalogue

- **Wikidata** ([catalogue page](sources/wikidata_catalogue.md)). Catalogues are on by default: a
  daemon whose configuration has no `[catalogues]` section and sets `http.contact` starts the
  Wikidata importer, which downloads the 104 GB dump once the disk is free. An instance that
  should import nothing sets `[catalogues] enabled = false`; one that should import other
  catalogues but not Wikidata sets `[catalogues.wikidata] enabled = false`, or `limit = N` for a
  part of it. An update asks the changed items' sitelinks first and reads in full only those with
  a Wikipedia article or a label the catalogue keeps: 24 items a second measured on one window of
  real changes, so a median day of changes (about 240,000 items) in about 3 hours, and the 2-3 days
  a first import starts behind in 6-8 hours (estimates).
- **MusicBrainz** ([catalogue page](sources/musicbrainz_catalogue.md)). An update writes only the
  cards that changed (0.9% of a sample's cards from one dump to the next). With a `limit`, a run
  reads the release file until the limit is full, then the release-group and artist files until
  it has found every item the subset needs, which is most of those files (1.2 GB and 1.8 GB).
- **OpenStreetMap** ([catalogue page](sources/osm_catalogue.md)). With the default
  `catalogues.osm.region = "planet"`, a daemon with the catalogue on downloads the planet file as
  soon as `catalogues.dir` has the room: set a region (`europe/switzerland`) on a small or staging
  instance, or switch the catalogue off. The map cells' triggers take about 40% of a card write
  (Switzerland: 100,000 located cards in 7.7 s with them, 4.7 s without). Updates wait while a full
  import runs, so the lag can pass the 2-day alert when the planet's import (every 90 days) waits
  for the large-import turn or free disk. The ODbL dump adds one line per imported place, written
  from one repeatable-read snapshot: Switzerland's took 32 s and 21 MB compressed (measured; its
  400,839 lines are the 400,838 places and the one place the run's lookups had cached), so the
  planet's about 45 minutes and 2 GB (estimate). The dump directory holds two copies while one is
  written.
- **The text index** reads each import's new cards in the background
  ([text index](operations.md#text-index)): 6,400 cards a second measured.

### Update jobs

Each importer runs its updates in the daemon, on its own:

| Catalogue | Update | How often | `CatalogueUpdateLag` after |
| --- | --- | --- | --- |
| `osm` | The region's change files; a full import that reads a new file | Looked for hourly, a new file daily; the full import every 30 days (the planet: every 90) | 2 days |
| `wikidata` | The recent changes | Every 15 minutes, at once while more than an hour behind | 2 days |
| `musicbrainz` | The next dump | `LATEST` read hourly; two dumps a week | 5 days |
| `openalex` | A new snapshot's newer files; a full import of the first new snapshot 180 days after the last full import | Daily look at the manifest | 100 days |
| `podcastindex` | The weekly database, read whole | Daily look, a new file weekly | 9 days |
| `software` | Each registry's file, read whole | Daily | 2 days |
| `tranco` | The day's list, then the homepages that are due | The list daily; homepages as they fall due (at once while some are due); a homepage again after 90 days | 2 days |

The alert fires once a catalogue's lag has been past its allowed lag for 15 minutes. While the
updates catch up after a full import (`pinakes_catalogue_catching_up` is 1), it waits up to 5
days longer ([allowed update lag](operations.md#allowed-update-lag)). `pinakesd catalogue status`
shows each catalogue's last import and update, how current it is and why it waits;
`pinakes_catalogue_update_lag_seconds` is the lag the alert reads.

### Memory

PostgreSQL and the text index read fastest from memory. Size RAM for the importers that can run
at once, the daemon, PostgreSQL's cache of the catalogue indexes (about half of `catalogue_card`'s
size) and the text index, which the daemon reads memory-mapped through the page cache. With less,
part of both is read from disk and lookups and text searches slow down by the disk's latency
(not measured).

## Small regional profile

A regional instance imports one region of OpenStreetMap, the first part of Wikidata and
MusicBrainz, and the software and Tranco catalogues. Set these in the configuration and leave the
rest as in `pinakes.example.toml`:

```toml
[http]
# contact = "operator@example.org" # Set your own address: the importers start once it is set.

[catalogues.wikidata]
limit = 100000
[catalogues.musicbrainz]
limit = 100000
[catalogues.osm]
region = "europe/luxembourg"
[catalogues.openalex]
enabled = false
[catalogues.podcastindex]
enabled = false
```

OpenAlex has only `min_citations`; Podcast Index, software and Tranco have no size setting: the
daemon imports each of them whole ([sizes](#sizes)). This block switches OpenAlex and Podcast Index
off, because whole they need more than the rest of the profile (below).

### Measured

Measured on 2026-11-27 with a release build and PostgreSQL 16, on a shared 20-core host while
other builds ran (load average 40-60): times are one run each. Each catalogue was imported with
`pinakesd catalogue import NAME` while the daemon served searches. OpenStreetMap read the
Luxembourg extract and Wikidata its dump up to the limit. MusicBrainz read the first 143 MB of one
dump's files, OpenAlex, Podcast Index and crates.io the first 252-315 MB of their real files (all
with `--from`), F-Droid and Flathub their whole files, and Tranco its list and 45 homepages. The
run imported samples of OpenAlex and Podcast Index too.

| Catalogue | Read | Cards | First import | Import's peak memory | Database growth | One update |
| --- | --- | --- | --- | --- | --- | --- |
| `osm` | Luxembourg, 48 MB | 31,435 | 5.8 s (37 s with the download) | 131 MB | 46 MB | 2 daily change files: 0.7 s |
| `wikidata` | 340 MB of the dump, streamed | 100,000 | 181 s | 200 MB | 395 MB | 30 minutes of changes: 12 s |
| `musicbrainz` | 143 MB of one dump's files | 84,621 | 18 s | 158 MB | 150 MB | The next dump: 25 s |
| `openalex` | 252 MB of three works files | 8,063 | 2.2 s | 61 MB | 17 MB | A 42 MB newer file: 1.7 s |
| `podcastindex` | 315 MB of the archive | 845,813 | 215 s | 58 MB | 1.84 GB | The database again: 312 s |
| `software` | 348 MB (262 MB of the crates.io dump) | 262,236 | 86 s | 150 MB | 386 MB | The files again: 86 s |
| `tranco` | The list (22 MB) and 45 homepages | 10 | 23 s | 133 MB | 154 MB (the list's million sites) | 5 homepages: 1.2 s |
| All | 1.47 GB | 1,332,178 | 530 s | | 2.95 GB at the end | |

| Measure | Value |
| --- | --- |
| Daemon memory (RSS) | 55 MB at start; up to 412 MB while the text index read the new cards; 344 MB idle after the imports; 623-657 MB under a search load (4 clients, 37 searches a second); 891 MB at most, while the text index took in the updates' changes |
| Database (`pg_database_size`) | 2.95 GB; `catalogue_card` 2.76 GB, half of it indexes |
| Text index | 509 MB (382 B a card); 479 MB after the updates |
| `catalogues.dir` | At most 117 MB (OpenStreetMap's file and node index while it imports); 0.4 MB kept after |
| Text index behind an import | 16 s after Wikidata's, 132 s after Podcast Index's 845,813 cards |
| Search, busy host | Median 98 ms, 95th percentile 227 ms under the load above; 84 ms and 173 ms for one client |

The block above, run as configured, holds other amounts than this run (estimate): a database of
about 2.5 GB (Wikidata 0.4 GB, MusicBrainz 0.2 GB, OpenStreetMap 0.05 GB, software 0.5 GB and
Tranco 1.25 GB once its homepages are read) and a text index of about 0.5 GB. It downloads the
Luxembourg extract (48 MB) every 30 days and a change file a day, 340 MB of the Wikidata dump once,
about 0.1 GB of MusicBrainz's release file and most of its release-group and artist files (up to
3 GB) for each dump, twice a week, about 0.6 GB of software files a day, and Tranco's 22 MB list a
day and its homepages (about 140 GB in its first pass of about two days).

For the block above, plan on 2 vCPU, 4 GB of RAM and 20 GB of disk (estimate). The daemon peaked
at 891 MB with the importers in their own processes; with one running in it, about 1.1 GB
(estimate). The rest of the RAM holds the catalogue indexes and the text index. The disk holds the
database with room for updates, the text index twice during `reindex`, and the importers'
free-disk checks (1-2 GB each). OpenAlex and Podcast Index switched on and imported whole add
36-42 GB of database, 7.5 GB of text index and their own disk checks (32 GiB and 20 GiB under
`catalogues.dir`): about 4 vCPU, 32 GB of RAM (about 20 GB of catalogue indexes and 8 GB of text
index) and 140 GB of disk (estimate).

A bigger region grows only OpenStreetMap: its download, the node index (about twice the file) and
about 1.4 KB a card in PostgreSQL. Switzerland's 552 MB extract gives 400,838 cards and 0.56 GB.

## Staging and production

### Configuration

A staging instance takes a region and limited Wikidata and MusicBrainz, with the other catalogues
on:

```toml
[http]
# contact = "operator@example.org" # Set your own address (or PINAKES__HTTP__CONTACT): the importers start once it is set.

[catalogues]
enabled = true
dir = "catalogues" # The container image sets /var/lib/pinakes/catalogues.
[catalogues.wikidata]
enabled = true
limit = 1000000
[catalogues.musicbrainz]
enabled = true
limit = 1000000
[catalogues.osm]
enabled = true
region = "europe/switzerland"
[catalogues.openalex]
enabled = true
min_citations = 50
[catalogues.podcastindex]
enabled = true
[catalogues.tranco]
enabled = true
[catalogues.software]
enabled = true
```

A production instance imports every catalogue whole. These are the defaults, so a configuration
without a `[catalogues]` section does the same once `http.contact` is set:

```toml
[http]
# contact = "operator@example.org" # Set your own address: the importers start once it is set.

[catalogues]
enabled = true
dir = "catalogues"
[catalogues.wikidata]
enabled = true
[catalogues.musicbrainz]
enabled = true
[catalogues.osm]
enabled = true
region = "planet"
[catalogues.openalex]
enabled = true
min_citations = 50
[catalogues.podcastindex]
enabled = true
[catalogues.tranco]
enabled = true
[catalogues.software]
enabled = true
```

### What each instance needs

Estimates from the catalogues' [sizes](#sizes), the [small regional profile](#small-regional-profile)
and an OpenStreetMap import of Switzerland. Size PostgreSQL's data disk at 1.5 times the database,
for updates, a full import's rewrites and WAL; shared links add to it
([storage model](#storage-model)). RAM follows the [memory](#memory) rule.

| Need | Staging (estimate) | Production (estimate) |
| --- | --- | --- |
| PostgreSQL | About 40-50 GB: OpenAlex 28-32 GB, Podcast Index 8-10.5 GB, Wikidata 2-4 GB (its first million items are the best known, between the 1.6-2.1 KB average and 3.9 KB a card), MusicBrainz 1.4-1.8 GB, Tranco 1.25 GB, OpenStreetMap 0.6 GB, software 0.5 GB | About 190-230 GB: MusicBrainz 70-90 GB, OpenStreetMap 49 GB, Wikidata 35-45 GB, OpenAlex 28-32 GB, Podcast Index 8-10.5 GB, Tranco 1.25 GB, software 0.5 GB |
| Text index (`search.index_dir`) | About 9 GB; 18 GB during `reindex` | About 48 GB; 95 GB during `reindex` |
| `catalogues.dir` | About 75-80 GB: OpenAlex's full import waits for 32 GiB free; beside it Podcast Index's run waits for 20 GiB, a MusicBrainz update for 16 GB (16 KiB a card of its limit), the Wikidata and OpenStreetMap updates for 1 GB each, and software and Tranco for 1 GiB each | About 285 GB: the planet's import waits for 215 GB (the 96 GB file and the 115 GB node index), Wikidata's for 129 GB (its 104 GB dump), one at a time; beside it a MusicBrainz update waits for 40 GB, Podcast Index's run for 20 GiB, an OpenAlex update for 4 GiB, a Wikidata update for 1 GB, and software and Tranco for 1 GiB each |
| Dump directory (`osm.dump_dir`) | About 50 MB (two copies) | About 4 GB (two copies) |
| Downloads | OpenAlex 675 GB from S3 for each full import; Switzerland's 552 MB extract every 30 days and a change file a day (kilobytes to a few MB); Wikidata 3.4 GB streamed once; MusicBrainz up to 4 GB streamed for each dump (a limit still reads most of the release-group and artist files); Podcast Index 1.9 GB a week; software 0.6 GB a day; Tranco's 22 MB list a day and its homepages (about 140 GB in the first pass) | OpenStreetMap 96 GB every 90 days and a change file a day (about 130 MB); Wikidata 104 GB once; MusicBrainz 26 GB for each dump; OpenAlex 675 GB for each full import; Podcast Index 1.9 GB a week; software 0.6 GB a day; Tranco 22 MB a day and its homepages (about 140 GB in the first pass) |
| RAM | 48 GB: about 25 GB of catalogue indexes, 9 GB of text index, up to 6 GB for the importers that run at once, 1-2 GB for the daemon | 256 GB: about 95-115 GB of catalogue indexes, 48 GB of text index, up to 10 GB for the importers that run at once (the planet's import up to 4 GB), the daemon; the rest caches card data. 192 GB holds the indexes alone |
| vCPU | 8 | 16 |
| First import | About 3-3.5 hours: OpenAlex about 2 hours (bound by the read from S3), Wikidata 30-45 minutes, MusicBrainz 10-20 minutes, OpenStreetMap a few minutes, one at a time; Podcast Index and software beside them | 15-25 hours: OpenStreetMap up to 15 hours plus the download, Wikidata 4-5 hours, OpenAlex about 2 hours, MusicBrainz about an hour, one at a time |

Tranco's first pass of homepage reads takes about two days after the large imports (estimate).
Each catalogue's update jobs and their cadence are in [update jobs](#update-jobs).

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
to those names. The other catalogues read theirs from `dumps.wikimedia.org` and
`www.wikidata.org` (Wikidata), `data.metabrainz.org` (MusicBrainz), `openalex.s3.amazonaws.com`,
`public.podcastindex.org`, `tranco-list.eu`, `static.crates.io` (and `cloudfront-static.crates.io`,
which it redirects to), `f-droid.org` and `dl.flathub.org`; Tranco's homepage reads go to the listed
sites through the egress policy. Allow them where outbound traffic is filtered by host.

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
