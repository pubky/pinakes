# Pinakes monitoring

Scrape the separate admin listener. Its default loopback bind is intended for a
local Prometheus/agent; configure `admin_listen` explicitly for a trusted network.
No authentication or reverse-proxy trust is added by this service.

```yaml
scrape_configs:
  - job_name: pinakes
    static_configs:
      - targets: ['127.0.0.1:9090']
rule_files:
  - /path/to/pinakes/ops/alerts.yml
```

Import `grafana/pinakes.json` and select a Prometheus datasource. The six panels
show ingestion lag, fetch outcomes, changes by class, canaries, queue depth and
overdue refreshes. Grafana contact points remain operator configuration. Route only
`severity=critical` (`IngestionStalled`) to paging; other rules are warning/info.
No messages are sent or contact points provisioned by these files.

| Series | Meaning |
| --- | --- |
| `pinakes_ingest_lag_lines` | Observed Nexus length minus committed cursor |
| `pinakes_ingest_last_success_timestamp_seconds` | Last applied Nexus poll |
| `pinakes_ingest_epoch_resets_total` | Persistent epoch/truncation rebuild count |
| `pinakes_fetch_total{source,mode,outcome}` | Actual upstream requests; outcomes ok, not_modified, missing, unreachable, rate_limited, error, drift |
| `pinakes_fetch_duration_seconds{source}` | Network/body duration histogram, excluding worker rate waits |
| `pinakes_changes_total{source,class}` | Persistent accepted change counts, unaffected by retention |
| `pinakes_feed_held_total{source}` | Persistent quarantined container snapshots |
| `pinakes_source_cursor_resets_total{source}` | Persistent upstream resets |
| `pinakes_source_last_success_timestamp_seconds{source}` | Last successful upstream fetch |
| `pinakes_canary_ok{source}` | Latest executed canary result, 0/1 |
| `pinakes_canary_fields_missing{source}` | Missing declared fields in the last canary |
| `pinakes_canary_consecutive_failures{source}` | Additional gauge needed to alert after exactly three failed runs, independent of scrape cadence or restarts |
| `pinakes_queue_depth{resolver,state}` | Ready pending, currently leased or dead jobs; future refreshes are excluded from pending |
| `pinakes_refresh_overdue{resolver}` | Non-final resources past refresh time, including failed/dead jobs |
| `pinakes_resources{resolver,status}` | Current resource totals |

Fetch counters/histograms reset with the daemon. Ingestion, source and change
counters are restored from durable database state at scrape time. Sources and modes
come from manifests; arbitrary user hostnames do not create metric labels. OSM
lookup with 50 objects counts once; cache hits and local geo parsing count zero.
A response is `drift` when parsing otherwise succeeds but an expected field is
absent. Empty Nominatim lookup results count as missing. Redirects and oEmbed calls
are separate actual requests under the web manifest. Canaries count in fetch totals
and use the same workers and host-rate limiter as ordinary resolutions.

The rules encode the design's starting thresholds. `IngestionStalled` has separate
rules for 15 minutes without success and 10 minutes above 1000 lag, avoiding an
extra delay on the first condition. `CanaryDown` uses the durable failure streak,
not repeated Prometheus scrapes of one failed run. For `FeedHeld`, `CursorReset`,
`SchemaDrift` and `RateLimited`, a new event stays visible for an hour.

Run `scripts/check-ops` for promtool syntax validation. `scripts/test` also parses
the YAML/dashboard and checks every referenced metric against an initialized
registry, so a renamed/unregistered metric fails CI even without promtool.
