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

Import `grafana/pinakes.json` and select a Prometheus datasource. The nine panels
show ingestion lag, fetch outcomes, changes by class, canaries, queue depth,
overdue refreshes, provider key refusals, catalogue cards and the update lag of the catalogues
that are on. Grafana contact points remain operator configuration. Route only
`severity=critical` (`IngestionStalled`, `MetricsMissing`, `IngestPanics`, `ApiErrors`) to paging;
other rules are warning/info.
No messages are sent or contact points provisioned by these files.

| Series | Meaning |
| --- | --- |
| `pinakes_ingest_lag_lines` | Observed Nexus length minus committed cursor |
| `pinakes_ingest_last_success_timestamp_seconds` | Last applied Nexus poll |
| `pinakes_ingest_epoch_resets_total` | Persistent epoch/truncation rebuild count |
| `pinakes_ingest_panics_total` | Polls whose batch work panicked outside a feed line; the cursor holds until it is fixed |
| `pinakes_ingest_rejected_total{reason}` | Skipped feed lines from committed batches; fixed reasons `unknown_kind`, `malformed`, `author`, `label`, `object_path`, `timestamp`, `uri`, `never_fetched` (a post in a namespace Pinakes never fetches: `social`, `pubky.app`, decoded), `collection_items` (a new item of a collection post that already holds 100), `panic`; resets on daemon restart |
| `pinakes_fetch_total{source,mode,outcome}` | Actual upstream requests; outcomes ok, not_modified, missing, unreachable, blocked, rate_limited, error, drift |
| `pinakes_fetch_duration_seconds{source}` | Network/body duration histogram, excluding worker rate waits |
| `pinakes_changes_total{source,class}` | Persistent accepted change counts, unaffected by retention |
| `pinakes_feed_held_total{source}` | Persistent quarantined container snapshots |
| `pinakes_source_cursor_resets_total{source}` | Persistent upstream resets |
| `pinakes_source_last_success_timestamp_seconds{source}` | Last successful upstream fetch |
| `pinakes_canary_ok{source}` | Latest executed canary result, 0/1 |
| `pinakes_canary_fields_missing{source}` | Missing declared fields in the last canary |
| `pinakes_canary_consecutive_failures{source}` | Failed canary runs in a row, kept across restarts, so an alert counts runs, not scrapes |
| `pinakes_queue_depth{resolver,state}` | Jobs due (`pending`), currently leased or dead. Pending is counted by the claim's own condition: future refreshes are not in it, and neither is a derived link whose derivation has ended and that nobody references (it is not read while nothing keeps it, and stays until its cleanup). The rows of a route that is switched off (`[web]`, `[pubky]`, `[geo]` or `[osm]` with `enabled = false`) are counted: ingestion still stores that route's links, and no worker claims them |
| `pinakes_refresh_overdue{resolver}` | Non-final resources past refresh time, including failed jobs but excluding dead letters. A link never read has no refresh time and is not counted here: it is `pending` in `pinakes_queue_depth` |
| `pinakes_group_budget_resources{state}` | Resources whose last grouping run parked a key that alone exceeded the work cap (`parked`), stopped on the run cap and retries hourly (`stopped`), or settled after 24 stopped runs on unchanged input (`settled`) |
| `pinakes_group_key_conflict_resources` | Resources whose last grouping run refused a join that would put two MusicBrainz IDs, Q-ids or Discogs IDs of one kind in a group (usually a wrong ID on a hub) |
| `pinakes_resources{resolver,status}` | Current resource totals |
| `pinakes_api_panics_total` | API requests (public or admin listener) whose handler panicked and were answered 500 |
| `pinakes_api_requests_total{listener,route,status}` | Requests answered, by listener (`public`, `admin`), route and status. The route is the path template (`/v1/resources/{id}`), `unmatched` for a path no route has; never the path asked. A 429 of the per-address budget and a 504 of `limits.request_timeout_secs` are counted. A tool call on `/mcp` counts once, there: the REST routes it calls are not counted again |
| `pinakes_api_request_duration_seconds{listener,route}` | Time from a request to its response's head (a streamed body, the dump, is not in it) |
| `pinakes_metrics_refresh_failures_total` | Scrapes whose read of the database failed or took over 8 seconds (one second less than `limits.request_timeout_secs` when that is shorter): `/metrics` still answered, with the database gauges it could not read at their last values. The queue's gauges (`pinakes_queue_depth`, `pinakes_refresh_overdue`) are read last, by a statement of their own: when that alone fails, the others (resources, canaries, ingestion lag and last success, groups, previews) are fresh and only the queue's are stale, with no series at all before their first read. Until the other gauges have been read once, `/metrics` answers 503 instead |
| `pinakes_preview_refused_total{limit}` | New previews `POST /v1/resolve` refused: `instance` (503, the hourly capacity `limits.preview_new_per_hour` taken) or `client` (429, one address's share `limits.preview_new_per_client_per_hour`, when set) |
| `pinakes_preview_backlog` | Resources nothing ever referenced (only previewed), each kept for `limits.preview_grace_hours`; derived children and venues are not counted |
| `pinakes_discovery_decisions_total{kind,decision}` | Optional metadata discovery decisions (a request may come from cache) |
| `pinakes_resolution_deferred_total{reason}` | Resolutions put off without a charged attempt, by reason: `missing_unconfirmed` (a missing answer, or a social page without its tags on a card that says more, waiting for the fetch that confirms it) |
| `pinakes_after_save_failures_total{resolver}` | Saved cards whose steps after the save failed (the alias its page names, an episode's pages), by resolver; the base routes' and each running resolver's series are at 0 from the start. The card stays as saved and no attempt is charged, so the link never dead-letters. A card saved `ok` is read again in full within `limits.max_backoff_secs` (an event platform's slower cadence, where it has one), so the step runs again; any other waits for its own next read. A steady rise is a step that fails every time; the log line `a step after the save failed` names the resource each time |
| `pinakes_also_on_service_errors_total{service,class}` | Failed also-on requests to a service (`wikidata`, `itunes`, `tidal`, `crossref`, …), by error class; the lookup goes on without that service |
| `pinakes_also_on_recording_key_conflicts_total{source}` | Also-on runs that keyed a recording another source keys differently on the same resource, by source; both keys stay and the resource joins no recording group |
| `pinakes_catalogue_cards{catalogue}` | Cards each catalogue holds (`wikidata`, `musicbrainz`, `osm`, `openalex`, `podcastindex`, `tranco`, `software`); every catalogue's series is at 0 from the start |
| `pinakes_catalogue_enabled{catalogue}` | Whether the catalogue is on (1) or off (0) in the configuration the daemon started with |
| `pinakes_catalogue_last_import_timestamp_seconds{catalogue}` | When its last full import finished; 0 before the first |
| `pinakes_catalogue_last_update_timestamp_seconds{catalogue}` | When its last update finished; 0 before the first |
| `pinakes_catalogue_update_lag_seconds{catalogue}` | Seconds its cards are behind upstream: since `current_as_of`, the upstream time of the data its last finished run left (a dump's date, the last change applied); 0 before its first full import has finished. An update that finds nothing new doesn't lower it |
| `pinakes_catalogue_max_update_lag_seconds{catalogue}` | How far behind upstream its cards may fall before `CatalogueUpdateLag` fires: its source's publication rhythm with a margin (2 days for Wikidata, OpenStreetMap, Tranco and the package registries; 5 for MusicBrainz; 9 for Podcast Index; 100 for OpenAlex) |
| `pinakes_catalogue_waiting_for_turn{catalogue}` | 1 while its full import waits for another large catalogue's full import (one runs at a time), else 0 |
| `pinakes_catalogue_import_errors_total{catalogue}` | Failed import and update runs; kept in the database (`catalogue_state.error_count`), so a restart does not reset it |
| `pinakes_source_token_refusals_total{source}` | Refusals of the operator's configured key, token or app credentials by its service, as each signals one (a 401; a 403 where the service uses it for a refused key, not GitHub's rate limit; a token endpoint answering anything but 200, 408, 429 or 5xx), never a 429 or 5xx. One series per keyed provider, each at 0 from the start: `courtlistener`, `discogs`, `github`, `osm` (Nominatim's 401 or 403), `podcast_index`, `spotify`, `ticketmaster`, `tidal`, `tmdb` |

Database metric refreshes are cached for 15 seconds. Overdue excludes dead letters. Every source
a running resolver reads has its series from the first scrape: fetch counters at 0, change, held
and reset counters at their stored totals.
Full JSON diagnostics are on the admin `/v1/info`; public info is redacted.

Fetch and API request counters/histograms, ingestion rejection, ingestion panic, API panic,
preview refusal, discovery, resolution deferral, failed step after a save, also-on error,
recording key conflict, metrics refresh failure and token refusal counters reset with the daemon.
Ingestion reset, source, change and catalogue error counters are restored from durable database
state at scrape time. Sources and modes
come from manifests; arbitrary user hostnames do not create metric labels. OSM
lookup with 50 objects counts once; cache hits and local geo parsing count zero.
A response is `drift` when parsing otherwise succeeds but an expected field is
absent. Empty Nominatim lookup results count as missing. Redirects and oEmbed calls
are separate actual requests under the web manifest. Canaries count in fetch totals
and use the same workers and host-rate limiter as ordinary resolutions.

The thresholds are defaults to tune per deployment. `IngestionStalled` has separate
rules for 15 minutes without success and 10 minutes above 1000 lag, avoiding an
extra delay on the first condition. `MetricsMissing` fires when Prometheus has had no Pinakes
series for five minutes (the daemon is down, or the scrape fails), so one failed scrape or a
restart is not reported as stalled ingestion (`reason="no_series"`). Its second rule,
`up{job="pinakes"} == 0` (`reason="target_down"`), fires for each target that has not answered
for five minutes: a Prometheus that scrapes several instances still has the others' series when
one is down. It reads the scrape job's name, `pinakes` above; change the rule when the job has
another name. When the only instance is down both rules fire, one alert each `reason`: group
them by `alertname` in Alertmanager to page once. `MetricsStale` fires when the
scrape answers but the database gauges, or the queue's alone, failed to refresh repeatedly over
fifteen minutes. `CanaryDown` uses the durable failure
streak, not repeated Prometheus scrapes of one failed run. For `FeedHeld`, `CursorReset`,
`SchemaDrift` and `RateLimited`, a new event stays visible for an hour. `WallUp` counts a
source's fetches refused with 401, 403 or 451 (`unreachable`) and those a recognised bot wall
answered (`blocked`). `QueueBacklog` has two rules: refreshes overdue, and jobs due and
unclaimed (`pinakes_queue_depth{state="pending"}`, which holds the links never read). On an
instance that switches a route off, that route's links stay due and its `pending` rule fires
once a thousand have arrived: leave that `resolver` out of the rule there.
`ApiErrors` (critical) is the share of public requests answered 500 or 504, `ApiRateLimited`
the share answered 429; both need twenty requests in five minutes.

`ProviderKeyRefused` (warning) fires when a provider refuses its configured key, one alert per
`source`: the key is mistyped, expired, revoked or its account lost access (Spotify answers 403
while the app owner has no Premium plan). Check or rotate that provider's key
([api-keys](../docs/api-keys.md)); keys are read only at start, so a new key needs a restart.
Spotify, TIDAL, TMDB, Ticketmaster and the podcasts also-on lookups keep asking with the key;
GitHub, CourtListener, Discogs and the Podcast Index card source stop sending a refused key for
an hour (GitHub's, CourtListener's and Podcast Index's links get web cards meanwhile, Discogs
reads without it) and then send it again. So every
provider's alert holds while its key keeps being refused and clears six hours after the last
refusal (a provider that takes a key back, a one-off refusal, or no lookup for that provider in
six hours). A refusal before Prometheus first scrapes a new target is its starting value, so the
alert shows only at the next refusal.

`CatalogueImportFailed` (warning) fires for an hour after a catalogue's import or update run
failed: `pinakesd catalogue status` shows the error, and the run is tried again from its cursor
every ten minutes, so a failure that persists keeps it firing. `CatalogueUpdateLag` (warning)
fires when a catalogue's cards are further behind upstream than its source's publication rhythm
allows (`pinakes_catalogue_max_update_lag_seconds`). Both read only catalogues
that are on and have finished a full import: a catalogue that is off raises neither, and the
failures of a first import show in `pinakesd catalogue status` and
`pinakes_catalogue_import_errors_total` alone. `CatalogueTurnBlocked` (warning) covers the one
stall that leaves: a large catalogue's full import that keeps failing keeps the large-import turn,
so another large catalogue waits for it. It fires when a catalogue has waited six hours for the
turn while a large catalogue's runs keep failing; fix the failing import's cause, switch it off
(its cut-off import then no longer holds the turn) or `pinakesd catalogue drop` it.

The catalogue rules match their series per `instance` and `job`, so a Prometheus that scrapes
several instances alerts for each on its own: `ops/alerts.test.yml` runs them with two scraped
instances (`promtool test rules`).

Run `scripts/check-ops`: it runs the test that parses the rules and the dashboard and checks
every metric they name, and every fetch outcome a rule matches, against an initialized registry,
then, when promtool is installed, `promtool check rules --lint-fatal` (a lint finding fails) and the rule tests in `ops/alerts.test.yml`. `scripts/test` runs it, so a renamed or unregistered
metric fails CI with or without promtool.
