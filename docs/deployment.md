# Deployment

A production checklist for the services, storage and network around `pinakesd`.
Configuration names come from [`pinakes.example.toml`](../pinakes.example.toml);
[API keys](api-keys.md) covers credentials, and [operations](operations.md) is the detailed runbook.

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
  **Its terms for what Pinakes keeps and republishes (cached place documents, the change journal
  and the ODbL dump) need Geofabrik's written confirmation before staging or production uses
  it.**

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
  shared by that proxy's users; enforce per-client limits at the edge. Do not expose admin routes
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
- Leave the Deezer API off. Links to Deezer from independent sources remain allowed; see
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
