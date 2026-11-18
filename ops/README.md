# Pinakes monitoring

Scrape `/metrics` on the separate admin listener (`admin_listen`, default
`127.0.0.1:9090`), meant for a local Prometheus or agent. A bind that is not loopback
(wildcard IPv4/IPv6 binds included) requires `admin_token` (`PINAKES__ADMIN_TOKEN`), and
startup logs a warning for it. With a token set, every admin request, `/metrics` included,
needs `Authorization: Bearer <token>` (in Prometheus, `authorization: {credentials_file: …}`);
terminate TLS at the network edge. The listener answers only a `Host` that is `localhost`, an IP
address or a name in `admin_hosts` (421 otherwise): a Prometheus that scrapes it by a name, such as
`pinakes:9090` from another container, needs `admin_hosts = ["pinakes"]`.

```yaml
scrape_configs:
  - job_name: pinakes
    static_configs:
      - targets: ['127.0.0.1:9090']
rule_files:
  - /path/to/pinakes/ops/alerts.yml
```

Import `grafana/pinakes.json` and select a Prometheus datasource. The seven panels
show ingestion lag, fetch outcomes, changes by class, canaries, queue depth,
overdue refreshes and provider key refusals. Grafana contact points remain operator configuration. Route only
`severity=critical` (`IngestionStalled`, `IngestPanics`) to paging; other rules are warning/info.
No messages are sent or contact points provisioned by these files.

| Series | Meaning |
| --- | --- |
| `pinakes_ingest_lag_lines` | Observed Nexus length minus committed cursor |
| `pinakes_ingest_last_success_timestamp_seconds` | Last applied Nexus poll |
| `pinakes_ingest_epoch_resets_total` | Persistent epoch/truncation rebuild count |
| `pinakes_ingest_panics_total` | Polls whose batch work panicked outside a feed line; the cursor holds until it is fixed |
| `pinakes_ingest_rejected_total{reason}` | Skipped feed lines from committed batches; fixed reasons `unknown_kind`, `malformed`, `author`, `label`, `object_path`, `timestamp`, `uri`, `never_fetched` (a post in a namespace Pinakes never fetches: `social`, `pubky.app`, `mapky.app`, `eventky.app`, decoded), `panic`; resets on daemon restart |
| `pinakes_fetch_total{source,mode,outcome}` | Actual upstream requests; outcomes ok, not_modified, missing, unreachable, blocked, rate_limited, error, drift |
| `pinakes_fetch_duration_seconds{source}` | Network/body duration histogram, excluding worker rate waits |
| `pinakes_changes_total{source,class}` | Persistent accepted change counts, unaffected by retention |
| `pinakes_feed_held_total{source}` | Persistent quarantined container snapshots |
| `pinakes_source_cursor_resets_total{source}` | Persistent upstream resets |
| `pinakes_source_last_success_timestamp_seconds{source}` | Last successful upstream fetch |
| `pinakes_canary_ok{source}` | Latest executed canary result, 0/1 |
| `pinakes_canary_fields_missing{source}` | Missing declared fields in the last canary |
| `pinakes_canary_consecutive_failures{source}` | Failed canary runs in a row, kept across restarts, so an alert counts runs, not scrapes |
| `pinakes_queue_depth{resolver,state}` | Ready pending, currently leased or dead jobs; future refreshes are excluded from pending |
| `pinakes_refresh_overdue{resolver}` | Non-final resources past refresh time, including failed jobs but excluding dead letters |
| `pinakes_group_budget_resources{state}` | Resources whose last grouping run parked a key that alone exceeded the work cap (`parked`), stopped on the run cap and retries hourly (`stopped`), or settled after 24 stopped runs on unchanged input (`settled`) |
| `pinakes_group_key_conflict_resources` | Resources whose last grouping run refused a join that would put two MusicBrainz IDs, Q-ids or Discogs IDs of one kind in a group (usually a wrong ID on a hub) |
| `pinakes_resources{resolver,status}` | Current resource totals |
| `pinakes_api_panics_total` | API requests (public or admin listener) whose handler panicked and were answered 500 |
| `pinakes_preview_refused_total{limit}` | New previews `POST /v1/resolve` refused: `instance` (503, the hourly capacity `limits.preview_new_per_hour` taken) or `client` (429, one address's share `limits.preview_new_per_client_per_hour`, when set) |
| `pinakes_preview_backlog` | Resources nothing ever referenced (only previewed), each kept for `limits.preview_grace_hours`; derived children and venues are not counted |
| `pinakes_discovery_decisions_total{kind,decision}` | Optional metadata discovery decisions (a request may come from cache) |
| `pinakes_resolution_deferred_total{reason}` | Resolutions put off without a charged attempt, by reason: `missing_unconfirmed` (a missing answer waiting for the fetch that confirms it) |
| `pinakes_also_on_service_errors_total{service,class}` | Failed also-on requests to a service (`wikidata`, `itunes`, `tidal`, `crossref`, …), by error class; the lookup goes on without that service |
| `pinakes_also_on_recording_key_conflicts_total{source}` | Also-on runs that keyed a recording another source keys differently on the same resource, by source; both keys stay and the resource joins no recording group |
| `pinakes_source_token_refusals_total{source}` | Refusals of the operator's configured key, token or app credentials by its service, as each signals one (a 401; a 403 where the service uses it for a refused key, not GitHub's rate limit; a token endpoint answering anything but 200, 408, 429 or 5xx), never a 429 or 5xx. One series per keyed provider, each at 0 from the start: `courtlistener`, `discogs`, `github`, `podcast_index`, `spotify`, `ticketmaster`, `tidal`, `tmdb` |

Database metric refreshes are cached for 15 seconds. Overdue excludes dead letters.
Full JSON diagnostics are on the admin `/v1/info`; public info is redacted.

Fetch counters/histograms, ingestion rejection, API panic, preview refusal, discovery, also-on
error and token refusal counters reset with the daemon.
Ingestion reset, source and change counters are restored from durable database state
at scrape time. Sources and modes
come from manifests; arbitrary user hostnames do not create metric labels. OSM
lookup with 50 objects counts once; cache hits and local geo parsing count zero.
A response is `drift` when parsing otherwise succeeds but an expected field is
absent. Empty Nominatim lookup results count as missing. Redirects and oEmbed calls
are separate actual requests under the web manifest. Canaries count in fetch totals
and use the same workers and host-rate limiter as ordinary resolutions.

The thresholds are defaults to tune per deployment. `IngestionStalled` has separate
rules for 15 minutes without success and 10 minutes above 1000 lag, avoiding an
extra delay on the first condition. `CanaryDown` uses the durable failure streak,
not repeated Prometheus scrapes of one failed run. For `FeedHeld`, `CursorReset`,
`SchemaDrift` and `RateLimited`, a new event stays visible for an hour.

`ProviderKeyRefused` (warning) fires once a provider has refused its configured key since the
daemon started, one alert per `source`: the key is mistyped, expired, revoked or its account
lost access (Spotify answers 403 while the app owner has no Premium plan). Check or rotate that
provider's key ([api-keys](../docs/api-keys.md)) and restart Pinakes: keys are read only at
start, and the restart resets the count, which clears the alert. It stays up until then on
purpose, because GitHub, CourtListener and Discogs stop sending a refused key until restart (the
first two then give their links web cards), so an hour's window would clear while the source
is still off. A one-off refusal with a good key also clears on restart.

Run `scripts/check-ops` for promtool syntax validation. Without promtool it runs the Rust
YAML/metric-registry test instead. `scripts/test` also parses
the YAML/dashboard and checks every referenced metric against an initialized
registry, so a renamed/unregistered metric fails CI even without promtool.
