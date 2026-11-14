# Deployment

A production checklist for the services, storage and network around `pinakesd`.
Configuration names come from [`pinakes.example.toml`](../pinakes.example.toml);
[API keys](api-keys.md) covers credentials, and [operations](operations.md) is the detailed runbook.

## Container

The Dockerfile builds the release `pinakesd` binary for the requested platform. The Debian
runtime runs as UID/GID `10001:10001`, includes CA certificates, and exposes only port 8090.
The image sets `listen = "0.0.0.0:8090"`, `admin_listen = "127.0.0.1:9090"` and
`osm.dump_dir = "/var/lib/pinakes/dumps"` as `ENV` variables (`PINAKES__LISTEN`,
`PINAKES__ADMIN_LISTEN`, `PINAKES__OSM__DUMP_DIR`). Environment variables override the TOML file,
so these three keys in a mounted TOML file have no effect: change them with
`--env PINAKES__LISTEN=…`, `--env PINAKES__ADMIN_LISTEN=…` or `--env PINAKES__OSM__DUMP_DIR=…`.
The image contains no operator configuration or credentials. PostgreSQL and the other services
below run separately.

Build and check the local image on a machine with Docker and enough build space:

```sh
docker build --pull --tag pinakes:local .
docker run --rm pinakes:local --help
```

Prepare an absolute-path copy of `pinakes.example.toml`, set the Nexus and Nominatim URLs
and `[http] contact`, and enable only the sources you use. Create a protected environment file
from [`.env.example`](../.env.example), readable only by its owner (`chmod 600`). Set
`PINAKES__DATABASE_URL` to the provisioned PostgreSQL database, `PINAKES__ADMIN_TOKEN`,
and any provider credentials there. `DATABASE_URL` alone is a test-tool setting; the daemon
reads `PINAKES__DATABASE_URL`. Container loopback refers to the container itself: use reachable
service names for PostgreSQL, Nexus and Nominatim. Environment overrides win over the TOML file.

After an image has been published, replace `v0.1.0` below with its verified version or digest.
For a local build, use `pinakes:local` as `PINAKES_IMAGE`.

```sh
PINAKES_IMAGE=registry-1.docker.io/synonymsoft/pinakes:v0.1.0
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

The config mount must be readable by UID 10001. A new named dump volume inherits the image
directory's ownership. For an existing volume or bind mount, provision write access for UID/GID
10001:10001 before starting. Keep that volume across replacements and back it up; the daemon
writes the ODbL dump through SQL and does not require a PostgreSQL client in the image. Run
database backups with a separate PostgreSQL client matching the server major version.

Put a TLS proxy in front of the loopback-published public API. Port 9090 remains on loopback
inside the container; do not publish it or use host networking to reach it. Private monitoring
needs an explicitly reviewed admin network configuration, bearer token and allowed admin hosts
as described under [Network and secrets](#network-and-secrets). One daemon owns one database;
stop the old container before starting its replacement. Follow the schema and backup checklist
below before upgrades. CLI commands replace the daemon arguments after the image name:
`--config /etc/pinakes/pinakes.toml --migrate`, or, with the daemon stopped,
`--config /etc/pinakes/pinakes.toml reextract all`, using the same mounts and environment file.

## CI and image publishing

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and `v*.*.*` tags.
It provisions a disposable PostgreSQL 16 cluster named `pinakesci` from the signed PGDG repository.
h3 stays on the documented minimum, upstream 4.2.3 (the newest PGDG build of it, from the signed
PGDG archive once the main repository drops it). The cluster must report h3 4.2.3+ and PostGIS
3.4+, compared as numbers, and run the h3 function; then CI runs the full `scripts/test` gate and
`scripts/e2e --mock`. Workflow contracts, actionlint, ShellCheck, Hadolint and a whitespace check
of the change against `main` also run. Rust is
1.98.1 in CI, the image build and `rust-toolchain.toml`. Native amd64 and arm64 image jobs build without registry login
or publishing and start the actual release daemon against the real database. They check the public API, ODbL
dump volume, nonroot user and loopback admin binding. No public provider or test identity is needed.

Tag publication waits for both jobs. It uses the org's
[`build_and_push` action](https://github.com/pubky/ci-workflows/tree/a57f2ae6382a8519ba4aa09d58b706f6b68a4426/.github/actions/docker/build_and_push)
at an exact commit to build `linux/amd64` and `linux/arm64` on the corresponding standard hosted
runners (`ubuntu-24.04` and `ubuntu-24.04-arm`). GitHub supports these runners for private
repositories ([runner support](https://github.blog/changelog/2026-01-29-arm64-standard-runners-are-now-available-in-private-repositories/)).
The Dockerfile compiles within each target platform's Rust image and ignores the shared action's
`TARGETARCH` build argument. Both platform jobs must pass their real database/API smoke tests.
That release build is separate from the PR image build, so each platform job then pulls the image
it pushed and runs the same smoke test on it; the multiarch tags only move after both pass. Each
platform job provisions that database before it logs in to Docker Hub, so no registry credential is
on disk during package installs. The pushed tags, `<full-commit-SHA>-<run-ID>-amd64` and
`<full-commit-SHA>-<run-ID>-arm64`, belong to one workflow run: two tags on the same commit start
two runs, and neither can replace the bytes the other tested. Re-running failed jobs keeps the run
ID, so a re-run platform job replaces and re-tests its own tag before the manifest job reads it. A
failed smoke test leaves its run's tag in the registry, never promoted.

Before any registry login, the publish and manifest jobs check that `main` itself pointed to the
tagged commit, on its first-parent line (`scripts/ci/release-on-main`). A tag on a feature branch,
or on a commit that only reached `main` inside a merged branch, publishes nothing. In each of those
jobs the check peels the pushed SHA to its commit (`git rev-parse "$GITHUB_SHA^{commit}"`), requires
the checked-out tree to be that commit, and hands it on: the full-SHA tag and the per-platform tags
use only that commit. GitHub documents `GITHUB_SHA` for a tag push as the tagged commit; for an
annotated tag, such as the signed one below, the commit-named image tags still name the commit and
never the tag object. That check lives in the tagged commit's
own workflow, so it catches mistakes, not someone who can push tags. Owner prerequisites, not set by
this repository's files:

- Branch protection or a ruleset on `main` with these required status checks, without bypass, so
  every state on `main`'s first-parent line passed them, as `scripts/ci/release-on-main` assumes:
  `Full test gate and mock e2e`, `Build and smoke image (amd64)` and `Build and smoke image (arm64)`.
- A tag ruleset for `refs/tags/v*` that lets only release maintainers create, update or delete
  those tags.
- Optionally, a `release` Environment holding the two Docker Hub secrets, limited to `v*` tags
  (with required reviewers if wanted). Create it first, move the secrets into it, then add
  `environment: release` to the `publish` and `manifest` jobs: naming an Environment in the
  workflow creates it with no protection rules. `scripts/ci/test_contracts.py` refuses an
  `environment:` until then; change that contract in the same commit.

Provision repository or explicitly granted organization Actions secrets before publishing:

- `DOCKERHUB_USERNAME`: a Docker Hub account allowed to push `synonymsoft/pinakes`.
- `DOCKERHUB_TOKEN`: that account's token with write access to the image repository.

Publishing is configured; this document does not establish that an image has been published or
that the registry credentials are available. PR and `main` jobs need neither secret and never
write to the registry. The workflow creates no Git tags, release objects or deployments.
Only a pushed, valid `v`-prefixed SemVer tag (at most 128 characters) can publish:

| Git tag | Image tags under `registry-1.docker.io/synonymsoft/pinakes` |
| --- | --- |
| `v1.2.3` | `v1.2.3`, full commit SHA, `latest` |
| `v1.2.3-rc.1` | `v1.2.3-rc.1`, full commit SHA; leaves `latest` unchanged |

The shared action publishes the intermediate `<full-commit-SHA>-<run-ID>-<arch>` image tags
described above. Its `latest` and `sha_tag` inputs are literal `false`: metadata's `enable=false`
suppresses those tags even though the composite action's SHA-computation step treats the string
as truthy. Raw metadata tags avoid automatic `latest` tagging. After both platform jobs succeed,
a separate job checks the combined manifest's platforms and publishes the version/full-SHA tags,
adding `latest` only for a stable version. SemVer build metadata is accepted: `v1.2.3+build.9`
becomes image tag `v1.2.3_build.9`. The underscore keeps it distinct from prerelease names.
`latest` follows the last successfully published stable tag, not a
comparison of version numbers. Use an immutable version/digest for deployments.

For an authorized release, first update the workspace version, complete review and CI on that
commit, and confirm the registry prerequisites. These commands create and push the signed tag;
they do not deploy the image:

```sh
VERSION=v0.1.0
git tag -s "$VERSION" -m "Pinakes $VERSION"
git push origin "refs/tags/$VERSION"
# After the CI publish job succeeds, inspect the manifest for both platforms.
docker buildx imagetools inspect "registry-1.docker.io/synonymsoft/pinakes:$VERSION"
```

## What to host

| Service | Why | Starting capacity / planning estimate | Operational requirement |
| --- | --- | --- | --- |
| `pinakesd` | Public cards/search API and a separate private admin listener | 2–4 vCPU, 4–8 GB RAM for a small instance; load-test the actual resolver mix | One daemon per database is enforced by the instance lock. Provider budgets are process-local; several instances behind one IP share upstream quotas. |
| PostgreSQL 16–18 | Documents, references, queues, groups and change journal | See the storage model below | PostGIS 3.4+, `pg_trgm`, h3 4.2.3+; startup checks `h3_latlng_to_cell(point,integer)`. Package upgrades may also need `ALTER EXTENSION h3 UPDATE;`. |
| Self-hosted Nominatim | Pinakes hosting dependency for venue geocoding and OSM place details (`osm.nominatim_url`) | Global dataset: 64–128 GB RAM (128 GB recommended), about 1 TB NVMe; allow several days for import | [Nominatim hardware guidance](https://nominatim.org/release-docs/5.2/admin/Installation/). Public Nominatim is only for small development runs. |
| Nexus | Reference feed and social/reach reads (`nexus_url`) | An existing compatible Nexus deployment | Trusted fixed upstream, not an arbitrary URL from a resource. Optional authorization comes from the environment. |
| Persistent dump directory | `osm.dump_dir`, served at `/v1/dumps/osm_place.ndjson.gz` | About 1.5 KB per cached place uncompressed; allow two copies during replacement | Keep the compressed dump downloadable and preserve OSM attribution; a CDN can cache it by `ETag`. |
| Prometheus and Alertmanager | Evaluate admin `/metrics` and the supplied alerts | Small deployment; retention and scrape frequency set disk use | Private scrape path; [ops setup](../ops/README.md) includes alerts and a Grafana dashboard. |
| MusicBrainz mirror (optional) | Higher music-lookup throughput via `also_on.sources.musicbrainz.base_url` | Size for the chosen mirror/search-index components, not a fixed 100 GB allowance | The hosted API's IP allowance is shared; a mirror is optional, not another provider key. [Server documentation](https://musicbrainz.org/doc/MusicBrainz_Server). |
| Photon (later, for clients) | Client place-search type-ahead, not called by Pinakes | Regional or planet index; provision against the selected [Photon dump](https://github.com/komoot/photon#setting-photon-up-with-the-release-binaries-and-extracts) | Clients call it directly; do not add a Pinakes autocomplete proxy. |
| Map tiles (later, for clients) | Client map drawing, not called by Pinakes | A regional/planet PMTiles file plus CDN/static storage; capacity depends on the selected build | MapLibre + self-hosted [Protomaps PMTiles](https://docs.protomaps.com/deploy/) is an option. Do not rely on OSM's donation-funded tile servers for production capacity; their [usage policy](https://operations.osmfoundation.org/policies/tiles/) is not a blanket ban on all production use. |

## Nominatim

Pinakes geocodes venues and reads OSM place details from one Nominatim, at `osm.nominatim_url`;
there is no other geocoding provider and no failover. Run one of:

- **A self-hosted Nominatim** (sizes above): its URL carries no secret and may stay in the TOML
  file.
- **Geofabrik's hosted Nominatim:** a paid, daily-updated service with Nominatim's API
  ([Geofabrik geocoding](https://geofabrik.de/data/geocoding.html)). Its URL may carry the
  account's key, so treat the whole URL as a secret: set it as `PINAKES__OSM__NOMINATIM_URL` in
  the environment file, never in the TOML file. The key must be part of the URL's path (a base
  with user info, a query or a fragment is refused at startup). Pinakes logs a request by its path
  below the base alone (`lookup`, `search`), shows the URL as `[REDACTED]` in debug output and
  serialization, and never in `/v1/info`. Set `osm.requests_per_second` to the plan's allowance.

The public `nominatim.openstreetmap.org` is for a small development run only
([operations](operations.md#deployment-and-egress)).

## Storage model

Staging and production can share read-only self-hosted datasets/services, such as Nominatim or
an optional MusicBrainz mirror, where their capacity, data licence and access rules permit it.
Budget staging traffic so it cannot consume production's provider allowance or service capacity.
Photon and map tiles remain separate client services, not Pinakes startup requirements; a
MusicBrainz mirror is optional too. Sharing a read endpoint is not permission to share writable
databases, admin credentials or environment/security boundaries. See [credential sharing](api-keys.md#optional-provider-credentials).

**Measured baseline, 2026-11-02:** the public production graph had **225 Resource targets with
1,198 incoming TAGGED edges**. Tags on Post or User targets are not Pinakes external-link tags.
These counts are a sizing input, not a promise that the graph and a particular Pinakes database
contain the same rows. Replies, embeds, collections, app resources and previews also affect storage.

A running instance measured separately had **1,725 resource rows, 6,221 reference rows** and
**65,133,591 bytes** of database storage (`pg_database_size`). User tables and their indexes,
excluding PostGIS's fixed `spatial_ref_sys`, used **47,046,656 bytes**; a custom-format
`pg_dump --no-owner --no-acl` was **2,458,465 bytes**. This sample includes queues, groups,
occurrences, the change journal, OSM cache and indexes; it is not a per-card JSON size.

The table is an **estimate**, using decimal units, **30 KB per resource including references at
the measured mix, plus 20 MB fixed overhead**. Tag counts scale with the graph snapshot but are
not charged a second time. Provisioning uses 1.5× estimated database size plus a 3 GB WAL floor;
backup ranges assume 10–30% of database size, conservatively above the small sample's compression.
Longer journals, larger documents, high-reference resources, table bloat, replication slots and
WAL retention can dominate; at large scale measure them rather than extrapolating this sample.

| Scale | External links | Tags on those links | Estimated database | Estimated data + WAL provision | Estimated compressed backup |
| --- | --- | --- | --- | --- | --- |
| Baseline | 225 | 1,198 | ~27 MB | ~3 GB | ~3–8 MB |
| 100× | 22,500 | 119,800 | ~695 MB | ~4 GB | ~70–210 MB |
| 100,000× | 22.5 million | 119.8 million | ~675 GB | ~1.02 TB | ~70–210 GB |

Re-measure your own instance without exposing credentials:

```sql
SELECT pg_database_size(current_database());
SELECT count(*) FROM resource;
SELECT count(*) FROM reference;
SELECT sum(pg_total_relation_size(relid))
FROM pg_stat_user_tables WHERE relname <> 'spatial_ref_sys';
```

For a graph refresh, count `MATCH ()-[t:TAGGED]->(x:Resource)` and return `count(t)` plus
`count(DISTINCT x)`; do not use all network tags as external-resource references.
Back up off-host, test restores and retain operator curation (`group_override`) and the change
journal (`resource_change`): a Nexus replay cannot recreate those. Dump the entire database,
not just `resource`, and budget backup retention separately from the table's single backup.

## Network and secrets

- Terminate TLS at a reverse proxy for the public listener (`listen`, default port 8090).
  Keep `admin_listen` (default `127.0.0.1:9090`) private; an admin token is required off loopback
  and recommended on it (the daemon warns at startup without one). The admin listener answers only
  requests whose `Host` is `localhost`, an IP address or a name in `admin_hosts`, and answers 421
  to any other, so a web page that points its own name at this machine reaches nothing. A scraper
  that reaches it by a name (another container's `pinakes:9090`) needs that name in
  `admin_hosts = ["pinakes"]`.
- The public API ignores forwarded client-IP headers. Behind a proxy its direct-peer budget is
  shared by that proxy's users; enforce per-client limits at the edge. For the same reason leave
  `limits.preview_new_per_client_per_hour` at 0 behind a proxy: a share of new previews keyed on
  the proxy's address would cap the whole instance. Do not expose admin routes
  by forwarding the public listener to the admin listener.
- Public CORS defaults to `["*"]` without cookies or bearer credentials. Use exact origins or
  `[]` where appropriate; admin routes send no CORS headers.
- Allow required public HTTP(S) egress and PKARR relays from `pubky.pkarr_relays`. Deny private,
  loopback, link-local and metadata destinations for resource-controlled fetches at the network
  layer too. Pinakes checks/pins DNS answers and checks redirects itself.
- Nexus and Nominatim are deliberately trusted configured upstreams outside that resource-fetch
  policy. Protect their configuration and DNS; their URLs cannot contain user info, query or
  fragment. A protected Nexus uses `PINAKES_NEXUS_AUTHORIZATION`, never a credential-bearing URL.
- Set an operator contact, inject secrets through a protected environment, and keep tokens out of
  logs. Provision only the optional providers you use: [complete key guide](api-keys.md).
- The Deezer API is on by default; `[also_on.sources.deezer] enabled = false` turns it off. Links
  to Deezer from independent sources stay either way; see
  [source-off behaviour](operations.md#also-on).

## Day-two checklist

1. Before an upgrade, take and restore-test a backup; compare any schema change against a fresh
   schema on a disposable database. The reviewed procedure must also handle SQLx migration
   metadata and prove that the actual target binary starts against the upgraded copy
   ([details](operations.md#database)). Never recreate a production database as an upgrade shortcut.
2. Stop the daemon before `pinakesd --config <path> reextract <route…|all|also-on>`;
   restart it afterward. Follow [database and re-extraction operations](operations.md#database)
   for the exact routes and queues affected.
3. Check public `/v1/info`, representative cards/searches, private admin info and queue progress.
   Process health alone does not verify ingestion or credentialed enrichment.
4. Scrape private `/metrics`, evaluate the [supplied rules](../ops/README.md), and alert on stalled
   ingestion, dead letters, queue backlog and canary failures. `canary.enabled` controls periodic
   real provider requests; review the platform canary notes before enabling them.
5. Keep OSM attribution and the dump available. Do not put caches in front of deleted social
   excerpts that outlive the service's refresh/purge behaviour. Use source-specific attribution
   and retention rules from the [source catalogue](sources/README.md), not assumptions that every
   provider's data has the same licence.
