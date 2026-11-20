# Operations

Start with [deployment and storage sizing](deployment.md) for what to host and the
[complete API-key guide](api-keys.md) for credentials, signup links and key-free fallbacks.
This runbook covers the configuration and day-two behaviour; provider details live in the
[source catalogue](sources/README.md).

## Configuration

`pinakes.example.toml` lists the operator choices: addresses and credentials, each resolver's
and feature's `enabled` switch, the event platforms (`enabled`, `key`), egress allow and deny
lists, and the public API's budget, CORS origins and Swagger UI. Every cap, timeout, budget,
backoff and refresh tier is a limit: it has a default and can be set in an optional `[limits]`
section (or as `PINAKES__LIMITS__<NAME>`). The web limits cover the web resolver and the event
routes on it; the resolver limits apply to pubky and to each card resolver.

| Limit | Default | Meaning |
| --- | --- | --- |
| `poll_secs` | `5` | Nexus feed poll interval in seconds. |
| `grace_days` | `7` | Days an unreferenced resource is kept before orphan cleanup. |
| `preview_grace_hours` | `24` | Hours a resource nothing ever referenced (one only previewed) is kept, at most `grace_days`. |
| `refresh_resource_secs` | `[604800, 172800, 21600, 3600]` | Resource refresh intervals by tier (seconds). |
| `refresh_container_secs` | `[86400, 43200, 21600, 3600]` | Container (calendar, feed) refresh intervals by tier (seconds). |
| `refresh_cursor_secs` | `300` | Cursor feed refresh interval (seconds). |
| `change_retention_days` | `90` | Days change lines are retained (at most 120 while a TMDB credential is set). |
| `canary_interval_secs` | `21600` | Adapter canary interval (seconds). |
| `web_concurrency` | `4` | Concurrent jobs of the web resolver, which the event routes share. |
| `web_requests_per_second` | `4.0` | Jobs per second of the web resolver and the event routes. |
| `resolver_concurrency` | `2` | Concurrent jobs per pubky or card resolver. |
| `resolver_requests_per_second` | `2.0` | Jobs per second per pubky or card resolver. |
| `resolver_ttl_secs` | `86400` | Default document lifetime (seconds) for web, pubky and card resolvers. |
| `music_enrichment_timeout_ms` | `8000` | How long a MusicBrainz card's enrichment may take, within the card's own read (milliseconds, 100–13000); past it, the card keeps its stored image and description. |
| `music_enrichment_retry_secs` | `3600` | The earliest a MusicBrainz card whose enrichment, or a Wikidata item's card whose Commons image, failed for a while (a timeout, a 429, a 5xx) is read again (seconds, 60–604800): it waits at least twice as long as since its last reading, up to its own refresh time, so a failure on a routine refresh waits for the next. |
| `music_image_width` | `640` | The width of the Commons thumbnail an enriched card or a Wikidata item's card asks for (pixels, 20–4096); Commons serves the next of its standard sizes (960 for 640). |
| `host_requests_per_second` | `1.0` | Requests per second to any one upstream host. |
| `host_rates` | `{}` | Per-host overrides of `host_requests_per_second`. |
| `html_head_bytes` | `4194304` | How far an HTML page past the 1 MB page cap is read (bytes); nothing in the page stops the read earlier. |
| `max_failures` | `5` | Failures before a resource dead-letters. |
| `timeout_grace` | `3` | Timeouts tolerated before they count as failures. |
| `backoff_secs` | `30` | First retry backoff (seconds). |
| `max_backoff_secs` | `3600` | Retry backoff ceiling (seconds). |
| `lease_secs` | `60` | Job lease (seconds). |
| `missing_ttl_secs` | `3600` | Re-check interval for missing resources (seconds). |
| `author_requests_per_second` | `1.0` | Resolution budget per author per second. |
| `max_label_bytes` | `128` | Longest accepted tag label (bytes). |
| `max_object_path_bytes` | `2048` | Longest accepted Pubky object path (bytes). |
| `osm_ttl_secs` | `2592000` | OSM place cache lifetime (seconds). |
| `osm_dump_every_hours` | `24` | Hours between two writes of the ODbL dump file. |
| `osm_dump_downloads_per_hour` | `6` | ODbL dump downloads per hour per client address. |
| `reach_timeout_secs` | `5` | Nexus reach deadline per search (seconds). |
| `related_taggers` | `200` | Taggers a related page samples: the authors of a resource's most recent tags, at most this many. |
| `related_window_days` | `365` | Days a related page looks back, for those taggers and their tags. |
| `related_refs_per_tagger` | `1000` | References a related page reads per sampled tagger (their most recent). |
| `related_min_shared` | `2` | Shared taggers a related row needs (its group's, when collapsed). |
| `related_tag_sample` | `2000` | Tags a related page reads per seed link (its most recent), from which the taggers are sampled. |
| `related_seed_links` | `32` | Seed links a related page samples: of the resource, its group's members and their aliases, the ones with the newest tags, at most this many. |
| `collapse_refs_per_member` | `10000` | References counted per group member when `collapse=group` picks a group's row. |
| `max_radius_m` | `200000.0` | Largest `near` radius (metres). |
| `api_cache_capacity` | `10000` | Entries the rate limiter remembers when an API request adds a client address (100–100000). It shares one table with the preview limiter; past it the oldest entry is forgotten with its used budget, so keep it above the clients, preview clients and previewed links seen in a minute. Also the client addresses the dump download limiter remembers per hour. |
| `request_timeout_secs` | `30` | Seconds a request may take on either listener before it is answered 504 (5–600). |
| `shutdown_grace_secs` | `30` | Seconds a stopping daemon gives open requests, running jobs and background passes to finish. |
| `reach_cost` | `6` | Budget units a search costs for each of `reach` and `boost`. |
| `related_cost` | `6` | Budget units a related search costs. |
| `preview_requests_per_minute` | `30` | Preview requests per minute per client. |
| `preview_uri_requests_per_minute` | `30` | Preview requests per minute per URI. |
| `preview_cache_capacity` | `10000` | The same table's size when a preview adds its client address and link (100–100000); past it the oldest entry (an API client's too) is forgotten with its used budget. Keep it equal to `api_cache_capacity`. |
| `preview_new_per_hour` | `1000` | New previews per hour for the whole instance; over it `POST /v1/resolve` answers 503. |
| `preview_new_per_client_per_hour` | `0` | New previews per hour one client address may take (0: no share); over it `POST /v1/resolve` answers 429. |
| `podcast_items` | `2000` | Podcast feed items read per scan (a show's list looks at its first 2000 and keeps the newest 1000). |
| `group_max_members` | `256` | Most members of one automatic group. |
| `group_max_distance_m` | `500.0` | Farthest two located members of one group (metres). |
| `group_max_graph_units` | `20000` | Units one grouping pass may read. |
| `group_max_graph_bytes` | `33554432` | Bytes one grouping pass may read. |
| `also_on_links_per_source` | `32` | Derived links one source keeps per resource. |
| `also_on_keys_per_source` | `16` | Join keys one source keeps per resource. |
| `also_on_facts_per_source` | `32` | Facts one source keeps per resource, at least 8: the cap cuts the many values of one name (a recording's ISRCs) before a name's one value, and 8 holds a recording's single-valued facts (its MBID, the MBID it was merged from, artist, length, original album, barcode). |
| `also_on_concurrency` | `2` | Resources whose sources run at once. |
| `also_on_retry_secs` | `300` | First retry of a failed source run (seconds); doubles up to an hour. |
| `event_max_line_bytes` | `65536` | Longest calendar content line (bytes). |
| `event_max_date_values` | `4096` | Most date values in one calendar property. |
| `event_feed_body_bytes` | `33554432` | Largest calendar or feed body (bytes). |
| `event_max_occurrences_per_series` | `50` | Most occurrences one series expands to. |
| `event_max_occurrences_per_feed` | `10000` | Most occurrences one feed yields. |
| `event_max_components` | `100000` | Most components one calendar may hold. |
| `event_max_revisions` | `10000` | Most revisions of one event (RECURRENCE-ID overrides). |
| `event_max_document_bytes` | `8388608` | Largest retained event document set per feed (bytes). |
| `event_max_recurrence_steps` | `100000` | Most recurrence steps one series may take. |
| `event_max_period_dates` | `100000` | Most dates one period of a recurrence rule may hold; a larger rule is not expanded. |
| `event_parse_timeout_ms` | `2000` | Calendar parse deadline (milliseconds). |
| `event_months_back` | `1` | Months of past occurrences kept in the horizon. |
| `event_months_forward` | `12` | Months of future occurrences kept in the horizon. |
| `geocode_score` | `0.8` | Lowest accepted geocoding score. |
| `held_retry_secs` | `60` | Retry interval of a held feed snapshot (seconds). |
| `same_event_incoming_kept` | `40` | The most events that may name one event in the same-event hint (each keeps its best twenty). |
| `same_event_drop_batch` | `500` | Events the same-event sweep drops per transaction. |
| `feed_max_changes` | `100` | Occurrence changes per snapshot before they become one summary line. |
| `feed_max_rows` | `10000` | Most stored rows of one feed. |
| `feed_max_new_keys` | `1000` | New keys one snapshot may add before it is held. |
| `feed_retention_days` | `30` | Days cancelled and past rows are kept. |
| `feed_max_holds` | `5` | Consecutive holds before a feed dead-letters. |
| `feed_cadence_floor` | `3600` | Shortest feed refresh interval (seconds). |
| `websocket_deadline_ms` | `2000` | Nostr relay websocket deadline (milliseconds). |
| `max_cursor_pages` | `10` | Pages one cursor feed fetch may read. |
| `max_indexer_pages` | `10` | Pages one Nostr indexer lookup may read. |
| `max_derived_resources` | `10000` | Derived links one adapter may hold across all parents. |
| `max_calendar_members` | `50` | Members one NIP-52 calendar may list. |
| `calendar_member_wait_hours` | `48` | Hours a NIP-52 calendar waits for a member that has never resolved. |
| `max_linked_resources` | `100` | Website links one BTC Map feed may derive. |

## Database

Supported: PostgreSQL 16–18 with PostGIS 3.4+ and `pg_trgm`.

`migrations/0001_schema.sql` is the whole schema. `pinakesd --config <path> --migrate` creates it
on an empty database and exits; the daemon does the same at startup, and so does `reextract`. A
database that already has it is left as it is.

Schema changes edit `0001_schema.sql`; this is not an incremental migration runner. A daemon
refuses a database built from a different schema version. For a production upgrade, back up and
restore-test first, apply a reviewed upgrade procedure while stopped, and prove the resulting
schema matches a freshly migrated disposable database. Schema equivalence alone is insufficient:
SQLx also validates the migration history in `_sqlx_migrations`, including the checksum of
`0001_schema.sql`. A reviewed, version-specific procedure must reconcile that metadata with the
target binary only after proving the intended schema/data transformation. Do not simply delete
the history or change its checksum to suppress `VersionMismatch`. On the restored upgrade-test
copy, run the actual target binary through migration and daemon startup and verify its API and
workers before production rollout. There is no general automatic in-place upgrade command.
Do not drop a production database to upgrade it: a feed replay cannot restore operator curation
or the change journal.

Only for a disposable local development database:

```sh
dropdb pinakes && createdb pinakes
pinakesd --config pinakes.toml --migrate
```

Tests create their own disposable databases.

A database advisory lock, the instance lock, allows one daemon per database. The daemon holds it on
a connection of its own for its whole life. Resolver, host, author, reach-cache and IP budgets are
process-local, and the lock keeps their guarantees. Every daemon version takes the same key, so a
second daemon on a database refuses to start. No other lock uses that key (`pinakes_core::locks`).
The lock is its session's: the daemon checks every ten seconds that the session is alive and
answers within ten seconds, and stops when it does not (a PostgreSQL restart, a dropped
connection, a database that does not answer), since another daemon or a `reextract` could then
start beside it.

**Stopping and restarting.** `pinakesd` stops with a non-zero status when one of its background
tasks ends by itself or panics (ingestion, grouping, maintenance, the ODbL dump, a queue worker,
a listener, the instance lock's check), or when a hint pass or also-on panics (those end by
themselves when switched off): a daemon that kept answering with a dead loop would look healthy
and fall behind. Run it under a supervisor that starts it again (`Restart=on-failure`, a
container restart policy). On SIGTERM or SIGINT it stops taking work, lets open requests,
running jobs and background passes finish for up to `limits.shutdown_grace_secs`, closes its
database connections (up to 5 seconds more) and exits 0, or non-zero when a task panicked while
stopping; what still runs after the grace is dropped (a job cut off there is read again when its
lease runs out). Give the supervisor's stop timeout more than the two together
(`TimeoutStopSec`, `stop_grace_period`). A signal during startup, `--migrate` or `reextract`
stops them too (`--migrate` and `reextract` then exit non-zero).

**Re-extraction.** A parser or routing change reaches stored documents through one command, run
while the daemon is stopped (it takes the instance lock):

```sh
pinakesd --config <path> reextract <route…|all|also-on>
```

It re-runs the current classification on the selected rows (all rows, or those routed to or
classified as one of the named routes or card kinds) and re-queues them at once, in batches of
1000. Each loses its HTTP validators and, on the OSM route, its `osm_place` cache entry, so the
next fetch re-extracts. A row entering or leaving the OSM route swaps its identity envelope and
becomes pending, as does an OSM-host URL moving to web. Leases are fenced by a generation bump.
Aliases have no route and are not re-queued; one that still holds a failure (an error, a dead
letter, a retry count) gets an alias's state, `pending` (or `ok` if it was read) with none, since
its root answers for it. A row whose URI now has a static alias root (a new tracking parameter,
say) becomes an alias of it when the route it is stored on is selected, and spellings of one
link can be stored on different routes: Spotify's `open.spotify.com/embed/…` is on `media` (`web`
with `[cards.media] enabled = false`) and `embed.spotify.com` on `web`, so folding both takes
`reextract web media` (or `all`). A selected alias (by the route its URI classifies to) whose URI a
rule now gives another root moves to that root. One that no rule names any more (a source that
read it as another object's spelling now reads it at its own address) answers for itself again:
it is re-queued on its route, regrouped and rescored with its old root, and its `aliased` change
line names its own ID. Every release comes before any move, so a spelling whose rule names a
released link moves to that link, never to the root the link had. An alias a page's canonical made
(basis `canonical`) or an official API's answer made (basis `source`) is no rule of its URI and
stays, as does the alias of a link whose source is switched off (`[sources.x] enabled = false`
silences its rule; what it merged before stays merged). To fix the aliases an earlier build made,
run `reextract all`: the links concerned
classify to several routes (a Reddit comment's link to `media`, a LinkedIn post's to
`official`). The selected rows' [also-on](#also-on) lookups are
queued too: a card that comes back unchanged would not run them again, so results read by an
older version would stay until they expire. `reextract also-on` queues every resource's also-on
lookups without re-reading any card; it can be combined with routes. Every run first scrubs
what rows stored before a scrub ran still keep of a social post that is gone or whose author
opted out, even when its refetch keeps failing. A post is gone when its stored card says
`missing`, whatever status a failed refresh gave its row since, and a gone post is treated as
gone even when its author had opted out. A gone post's card keeps nothing of the post
(only its link, source, status and times, and the one type every gone post has,
`SocialMediaPosting`), its change lines and also-on results are
deleted, and it is regrouped. An opted-out post's lines lose their documents. The aliases of
either keep only their `aliased` line: their cards, other lines and also-on results are deleted.
It prints `scrubbed N posts gone or opted out`, then the re-queued count.

**Dead letters.** To retry one by hand:

```sql
UPDATE resource SET dead_letter=false, attempts=0, next_attempt_at=now(),
  lease_until=NULL, generation=generation+1 WHERE id='<resource_id>';
```

**Grouping rebuild.** The daemon stores its grouping policy (its limits and mode) and
re-queues every membership when it changes, so a changed `groups.cross_container_occurrences`
takes effect as that queue drains. To rebuild by hand:

```sql
DELETE FROM group_input;
INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;
```

**Same-event hint rebuild.** The hourly sweep reads again any event in the window whose start,
place or name changed unseen. To recompute every pair (cards keep their current pairs until each
event is read again):

```sql
UPDATE same_event_unit SET fingerprint='';
INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT resource_id FROM same_event_unit ON CONFLICT DO NOTHING;
```

**Same-recording hint rebuild** (after a change of the title normaliser, `matching.rs`), the
same way:

```sql
UPDATE same_recording_unit SET fingerprint='';
INSERT INTO same_recording_dirty(resource_id) SELECT resource_id FROM same_recording_unit ON CONFLICT DO NOTHING;
```

## Deployment and egress

For container commands, config/secret injection and the persistent dump mount, see
[container deployment](deployment.md#container).

Web fetches bypass environment proxies, validate every DNS answer, pin the checked addresses and
repeat the policy at every redirect (three hops; HTML meta refresh and literal JavaScript
redirects count). `egress.allow_hosts` and `egress.deny_hosts` take exact names or
`*.example.org`; they never override the IP policy, and deny wins. A rule may be written in
Unicode or punycode, in any case and with a trailing dot or full stop (`.`, `。`, `．`, `｡`,
`%2e`; `faß.de` and `xn--fa-hia.de` are one rule); an IP address is read as a URL reads it
(`10.0.0.1`, `[::1]`; `8.8.8` and `8.8.8..` are `8.8.0.8`). A rule and the host a request names
are read by the same function, so they meet however either is written. A rule that names no host
stops startup, naming the rule. Pages are capped at 1 MB of
decoded body; an oversized page keeps its head metadata with a `truncated` warning. An HTML page
past the cap is read on to `limits.html_head_bytes`, whole when it ends before that and cut there
otherwise: nothing in the page's text stops the read earlier, since only the parser can say where
a head ends (a `</head>` in a script is not one). A read that cuts stops as soon as it reaches
its cap, so a sender that stalls there costs no timeout. An image or PDF past the
cap is cut too, never refused: its card comes from its URL. Every HTML parse (pages, and HTML
inside API answers) runs in a child process, `pinakesd parse-worker` (the daemon itself; one
that cannot run it does not start). All the parses of one job (a page's extraction, a source's
answer with many HTML descriptions, a calendar's events) share one worker and its limits: 1 s of
CPU time (10 s in a debug build), 1 GiB of memory and a wall-clock deadline. Past any of them, or
as soon as the job's caller stops waiting, the worker dies, every later parse of the job fails
at once, the parse slot is freed, and the resource is recorded as `too_complex`: final, with no
card (an earlier card is cleared, and the change recorded), until `pinakesd reextract` revives it
(the daemon logs `an HTML parse did not end within its limits`, once per job). Text without
markup (no `<`) is parsed in the daemon, in linear time, without a worker, but only while the
job is within its limits: once it is over, it parses no plain text either.
The tree a worker answers with keeps no node more than 256 elements deep and at most 500,000
nodes (of 149 real pages, the most has 37,186). Before the parse, a linear scan cuts the text
at the first obvious cap: 50,000 start tags, 256 open elements, 64 attributes on one tag (end
tags too), an attribute value of 1 MiB, a tag name of 256 bytes (real pages stay far below each:
at most 16,440, 62, 29, 121 KB and 27 bytes over 149 pages). The scan is a cheap early cut; the
worker's limits are the guarantee. At most as many pages are parsed at once as the machine has
cores: a parse holds its slot until it ends, even after its request timed out. An RSS or
Atom feed read without a calendar's feed cap is cut there too (a long show's feed is 2–5 MB):
its channel and newest items come first, and its card reads up to the last whole item. Wikidata
entity reads may take 8 MB, since a much-edited item carries megabytes of claims, and so may
MusicBrainz answers (an artist or release with very many links). An answer over even that is
refused, never cut, and remembered as such for five minutes. The also-on sources then record only
what the link's URL proves (a MusicBrainz page its MBID or ISRC, a Wikidata item its Q-id) and
don't retry it; Wikidata items verified together that are over it together are read one by one;
a MusicBrainz page's card is read from the page. Requests
time out after ten seconds. Card images stay origin URLs and are never fetched; a link that is
itself an image resolves to an ImageObject card.

Discovery reads (`Egress::discover`: the also-on sources and hubs, oEmbed endpoints, feed and
calendar discovery, and a few page hooks and sources such as `feed`, `media_records`, Discogs and
GitHub; not the page fetch of web resolution or most official-API reads) share one in-process
cache (10,000 entries, 32 MiB), and concurrent readers of one request share one fetch. A success
is kept for the lifetime the reader asks for, raised to at least an hour; a failure for five
minutes, or, when the service named its wait (a 429's `Retry-After`), for that wait held between 1
and 300 seconds. Eviction of the least recently used entries can end any entry sooner. An
authentication failure (TIDAL's and Spotify's 401 and 403) is never kept, so a new token asks again, and a wait
for Pinakes's own host budget is not a failure.

Every request introduces itself as `pinakes/<version> (+https://pubky.app; <contact>)`, with
`http.contact` as the contact, or `pinakes/<version> (+https://pubky.app)` without one. There is
none by default, and the example configuration has only a commented placeholder. Set it to an
address the services you read can reach: some refuse requests without one, and the links of the
sources that need it (SEC EDGAR) wait until it is set. The User-Agent never names a code-hosting
site, which some services refuse as an undeclared tool. Event platform adapters send the same
User-Agent.

The official-API sources ([docs/sources](sources/README.md)) run on the `official` card route
(`[cards.official]`), each with its own switch, `[sources.<name>] enabled` (on by default;
`PINAKES__SOURCES__<NAME>__ENABLED=false`); a switched-off source's links get ordinary web cards
and its address rules no longer merge new links (links it merged before stay merged). A source
that reads with the operator's token (`github`: `[sources.github] token` or `PINAKES_GITHUB_TOKEN`;
`courtlistener`: `[sources.courtlistener] token` or `PINAKES_COURTLISTENER_TOKEN`) is on only with
one. Its token is a secret, sent only to its service's API and never through a redirect. A token
the service refuses (401; CourtListener's 403 too) switches the source off until restart, and
`pinakes_source_token_refusals_total{source}` counts each refusal. That series counts every keyed
provider's refusals (Discogs, Spotify, TIDAL, TMDB, Podcast Index and Ticketmaster too, as each
signals one: a 401, a 403 where it means a refused key, or a refused token request; never a 429
or 5xx), and the `ProviderKeyRefused` alert
(`ops/alerts.yml`) reads it. A source
lowers its service's host to the service's published rate (never above
`limits.host_requests_per_second`) unless `limits.host_rates` names the host, and a request that
would wait more than five seconds for its host (its slot, or the request ahead of it) is retried
later instead of timing out; a redirect within the host keeps the slot it has. After an upgrade
that adds a source, run `pinakesd reextract web official`: links already known get their source's
card.

Two clients are outside that policy. Nominatim and Nexus are trusted, operator-configured
upstreams at fixed URLs that no reference can change: their clients do not apply the web DNS
pinning or the private-address refusal (Nexus defaults to loopback), so configure their URLs and
DNS through trusted controls. Their Retry-After delays are capped at one hour. The public
Nominatim is for a small development run only: its [usage
policy](https://operations.osmfoundation.org/policies/nominatim/) requires an identifying
User-Agent, caching and at most one request per second, and `osm.requests_per_second` defaults
to 0.5 and is refused above 1 for a public host. Staging and production use their own
`osm.nominatim_url`, a self-hosted or a hosted Nominatim ([deployment](deployment.md#nominatim)).
Pinakes never proxies place search, autocomplete or reverse geocoding.

The ODbL share-alike download is a file: the daemon writes `osm_place.ndjson.gz` into
`osm.dump_dir` (default `dumps`, relative to the working directory) at startup when it is missing
or older than `limits.osm_dump_every_hours` (24), then at that interval. One writer per
directory: the writer holds `osm_place.ndjson.gz.lock` (a file lock), so daemons sharing the
directory take turns and one skips a file another has just written; holding it, the writer, and
the daemon at startup, remove the temporary files an interrupted write left. It reads one repeatable-read snapshot
without a statement timeout, compresses it into a temporary file of its own
(`osm_place.ndjson.gz.{pid}.{random}.tmp`) off the async workers, syncs it and renames it into
place, holding the lock until the rename and the directory's sync are done; a failed write, or
one refused because another daemon holds the lock, keeps the previous file and is retried after
five minutes. Shutdown cancels a write in progress. One cancelled while it writes its temporary
file keeps the previous file, and the next start removes the temporary file; one cancelled at its
rename still finishes it, holding the lock until it is done. The directory
must be writable by the daemon and hold two copies of the file while one is written: about 1.5 KB per cached place
uncompressed, several times less gzipped. Downloads read the file only (no database work),
`limits.osm_dump_downloads_per_hour` (6) transfers per address (`HEAD`, 304 and 416 are free);
a CDN or reverse proxy may cache it by its `ETag`.

### Pubky

Pinakes fetches a `pubky://` object through the egress policy above. It reads the owner's
`_pubky` PKARR record (following a delegation to the homeserver's key, at most seven deep) from
the PKARR relays in `pubky.pkarr_relays` (by default `https://pkarr.pubky.app` and
`https://pkarr.pubky.org`), asked together: a GET through the egress policy (an answer above
8 KB refused, whatever its content type), a 404 meaning no record. Pinakes checks every packet's
signature against the owner's key and uses the newest packet that verifies. A relay can withhold
a record (when every relay answers 404, the owner has no record for five minutes) but cannot
forge one, and Pinakes never uses a packet older than the newest it remembers for that key. That
memory is an accepted limit: it holds the newest timestamp of at most 10,000 keys, apart from the
cache, and forgets the oldest timestamps of uncached keys first; a forgotten key, or any key
after a restart, takes the newest packet the relays serve again. A relay that times out, is
refused by the policy or serves a packet that does not verify is a `connect` failure, retried
with the usual backoff and, like any failure, dead-lettered after `limits.max_failures`. When no
relay answered, because each one that failed refused or reset the connection (or Egress waited
for its host budget), the failure never reached an answer and never dead-letters.
Verified packets are kept for their smallest record TTL, between five minutes
and a day (10,000 keys at most). It then takes the first endpoint with a public HTTPS (ICANN)
domain and GETs the object path there with the `pubky-host` header, as Pubky clients address
ICANN endpoints. The usual rules apply: every DNS answer checked and pinned, private, loopback,
link-local and metadata addresses refused, HTTPS only, no proxy, the ten-second deadline, the 1
MB cap and the content-type allowlist, which for Pubky objects also admits
`application/octet-stream`, `text/plain` and unlabelled bodies (homeservers label objects by
magic bytes or extension, so JSON arrives as `application/octet-stream`). The object is read as
JSON whatever its declared type; one that is not JSON becomes a card with only its content type
and size (no card, for a kind an app's profile reads). Which objects are fetched and what their
cards hold is per app ([Pubky apps](pubky-apps.md)); after an upgrade that adds or changes an
app's profile, run `pinakesd reextract pubky`. Because the request carries a header, a redirect is refused rather than followed.
Refusals by the policy (a redirect, a content type outside the list, a non-public address, an
owner host that is not the canonical z32 key, a path that changes when read as an HTTPS path, an
endpoint that is an IP address rather than a domain) are stored as `policy` and dead-lettered at
the first attempt instead of retried. **A homeserver that publishes only key-pinned
(raw-public-key TLS) endpoints is unreachable** (`connect`, with a message saying so).

Homeservers count against the per-host budget (`limits.host_requests_per_second`, one request a
second by default) and the egress allow and deny lists like any host. Most objects live on a few
homeservers, so a busy one may need its own rate in `limits.host_rates`
(`"homeserver.example" = 4.0`). The Pubky resolver's own limits
(`limits.resolver_concurrency`, `limits.resolver_requests_per_second`: 2 and 2.0) and the author
budget apply on top. The relays count against the per-host budget too; the record cache keeps
their load to one request per owner and TTL.

### Clients and budgets

Every public `/v1` route shares the direct-peer IP budget (`api.requests_per_minute`), and
previews have their own per-IP and per-URI budgets. Forwarded headers are deliberately ignored:
behind a reverse proxy the proxy's address gets the aggregate budget, so set client-IP limits at
the edge.

CORS: `api.cors_allow_origins` (default `["*"]`, safe because the public API takes no cookies or
tokens and writes nothing but budgeted previews) lets browser apps on other origins call the
public listener. List exact `scheme://host[:port]` origins to restrict it (no wildcard patterns
such as `https://*.pubky.app`), or `[]` to send no CORS headers. Preflights get 204 with
`Access-Control-Allow-Methods: GET, POST`, `Access-Control-Allow-Headers: content-type` and
`Access-Control-Max-Age: 600`, and spend the per-IP budget. In `*` mode every response, 429s and
requests without an `Origin` included, carries `Access-Control-Allow-Origin: *` and exposes
`Retry-After` and `ETag`, so a shared cache filled by a non-browser client still serves
browsers. With an origin list only allowed origins get the headers (echoed) and every response
adds `Vary: Origin`. No `Access-Control-Allow-Credentials` is sent, and the admin listener never
sends CORS headers.

The admin listener (`admin_listen`, default `127.0.0.1:9090`) requires `admin_token` (or
`PINAKES__ADMIN_TOKEN`) for a non-loopback bind; terminate TLS at a trusted reverse proxy. On
loopback the token is optional, and startup logs a warning without it: the host check below keeps
browser pages out, but any process on the machine can use a listener without a token.

It answers a request only when its `Host` (or HTTP/2 authority) is `localhost`, an IP address (any
port) or a name listed in `admin_hosts`; any other gets 421 `admin host not allowed` before the
token is checked. A request with no `Host` passes, as browsers always send one. This stops a page
that rebinds its own DNS name to this machine (or to an SSH tunnel's local end) from reading or
curating through the listener. A scraper that reaches the listener by a name, such as a Prometheus
in another container scraping `pinakes:9090`, needs `admin_hosts = ["pinakes"]`; each entry is a
bare lowercase host name, without scheme, port or wildcard.

Swagger UI (`api.swagger_ui`, default on) serves the vendored `utoipa-swagger-ui` bundle (Swagger
UI 5.17.14) at `/swagger-ui`; builds never download it, and a build environment must not set
`SWAGGER_UI_DOWNLOAD_URL`. Every `/swagger-ui` response carries `X-Content-Type-Options: nosniff`
and a CSP of `default-src 'self'` with no inline script; the bundle's one inline `<style>` is
allowed by its SHA-256 hash, which a test recomputes from the served bundle. The validator badge
is off. Swagger UI shares the `/v1` per-IP budget.

The agents' guide (`/llms.txt`, `/skill.md`, the index at `/`) and the MCP endpoint (`POST /mcp`,
`api.mcp`, default on) are on the public listener under the same per-IP budget
([api](api.md#agents)). The MCP endpoint is stateless and read-only, takes no credentials and
accepts any `Host`; terminate TLS at the reverse proxy and pass `X-Forwarded-Proto`, so the guide
names the public `https` URL. Turning `api.mcp` off removes `/mcp` and its link from the index;
the guide stays.

## Ingestion

`pinakesd` polls the Nexus reference feed (`GET /v1/resources/changes`) every `limits.poll_secs`. Pinakes keys each line on its
`uri`, normalized and hashed into its own ID (Nexus's `resource_id` is not read); a URI that does
not normalize is rejected (`uri`). A line's author must be 52
lowercase z-base-32 characters; labels and object paths must be non-empty, NUL-free and within
`limits.max_label_bytes` and `limits.max_object_path_bytes`. A page is parsed line by line: a line
of a kind this build does not read (`unknown_kind`, a newer Nexus's) or one it cannot parse
(`malformed`) is skipped without its content in any log, and only the page's envelope (`lines`,
`next_cursor`) must be well formed. A bad line is skipped and counted in
`pinakes_ingest_rejected_total{reason}` (`unknown_kind`, `malformed`, `author`, `label`,
`object_path`, `uri`, `timestamp`, `never_fetched`, `collection_items`, `panic`) after the batch commits; the cursor still advances
past it. A reference is keyed as Nexus keys it: a tag by its author, `uri` and label (its time is
its latest put's `at`, the time Nexus gives the tag), so a del
naming another tag file than the put did (a user's `pubky.app` tags rewritten to `social/v1`, or
two files on one tag) deletes that tag, and a repeated put or del changes nothing more; a reply,
embed or collection item by its author, `object_path`, kind and `uri`; a post by its version file
([Posts](#posts)). `collection_items` is a new item of a collection post that already holds 100 (the specs' cap). Each line runs in a savepoint: a line whose processing panics is rolled back alone and skipped (`panic`; the log
line names its feed position only, never its content). A panic anywhere else in a poll (the
batch's score update or cursor write, say) is an error: the batch rolls back, the cursor holds,
`pinakes_ingest_panics_total` counts it and the poll is retried. A feed epoch change, or a
cursor beyond the feed's length, clears the reference projection and scores and restarts at
zero, after one `limits.poll_secs` wait; resource documents stay through the grace period.
The cursor is kept under `nexus_url` as written. After `nexus_url` changes (another Nexus, or
another spelling of the same one, such as a trailing slash) the new feed is read from zero and
the references read from the old one stay beside it: the daemon logs a warning at each start
that names the other feed's host. The other feed's cursor row also stays in
`pinakes_ingest_lag_lines`. To start over with the new feed alone, stop the daemon and
clear the projection as a feed reset does
(`UPDATE resource SET orphaned_at=coalesce(orphaned_at,now()); TRUNCATE reference, resource_score, feed_cursor;`):
the next start reads the configured feed from zero. A resource without references is
deleted `limits.grace_days` after it became orphaned. One nothing ever referenced (a link only
previewed with `POST /v1/resolve`) goes after `limits.preview_grace_hours` (a day) instead: its
orphan time is still the instant it was created, as a reference clears it and losing the last
one sets a later time. A derived child or a venue is kept as before, however it was created, and
when what held it is deleted it keeps the full grace counted from its creation, never the preview
grace.
The hourly cleanup deletes in batches of 100 until it has caught up, or for two minutes at
most. A Nexus outage leaves cached data served and polling retries.

`POST /v1/resolve` stores at most `limits.preview_new_per_hour` (1000) links the instance did not
know in any hour, whoever asks. Over it, a new link gets 503 with `Retry-After` (the seconds until
the oldest of the hour's previews is an hour old) and nothing is stored; a known link still
reads, and the per-client budget's 429 comes first. With
`limits.preview_new_per_client_per_hour` set (0, no share, by default), one client address takes
at most that many of them: over it, that client gets 429 with `Retry-After` and the others go on.
Set it only where clients reach the listener directly: the share is keyed on the connecting
address, as the per-IP budgets are, and behind a reverse proxy every client has the proxy's, so a
share would cap the whole instance. A place is kept only by the request that stored the link; one
whose request fails or goes away first is given back. The window is in memory and starts empty at
startup. `pinakes_preview_refused_total{limit}` counts the refusals, `instance` for the hour and
`client` for a share (alert `PreviewCapacityReached` on either),
and `pinakes_preview_backlog` the resources nothing ever referenced (not derived children or
venues).

### Posts

Apps' posts come on the same feed as `post` lines: the line's `uri` is the post itself,
versionless (`pubky://<author>/pub/<namespace>/v1/posts/{id}`), and its `object_path` the version
file written (`…/{id}/{editId}.json`). The URI must be a post in its author's own storage (else
`uri`) and the object path a version file directly in it (else `object_path`). Social events and
rsvps are indexed by Nexus. A `post` line in a namespace
Pinakes never fetches (`social`, `pubky.app`, and the old `mapky.app` and `eventky.app`, read
decoded, so `%73ocial` is `social`) is rejected (`never_fetched`), creating no resource or
reference and fetching nothing. Letter case is kept, as the resolver keeps it: `Social` is another
app's namespace. A namespace that is not one path segment once decoded (not UTF-8, an encoded `/`,
`.` or `..`) is `uri`. A post line is a reference of the post to itself, keyed by its version file as
a reply, embed or collection item is by its file: it keeps the post's card alive and counts nowhere (scores, trending, reference
filters). The post is queued as any `pubky://` object; a new version, or a deleted one, makes its
card due at once whatever its TTL. The Pubky resolver lists the post's folder newest first and reads
the latest version by its namespace and kind ([Pubky apps](pubky-apps.md#posts)); a post without
a version left is `missing`. A `kind:event` line, which Nexus reserves, is `unknown_kind`.

Search's `reach` and `boost` read the viewer's `following` and `friends` lists from the same
Nexus. `nexus_reach_set` (default `false`) says that Nexus also serves the reach-set route
(`GET /v1/user/{viewer}/reach-set?depth=2`), which `wot_2` reads; while it is off, `wot_2`
answers 501 and Nexus is never asked for it. Sets are kept in memory only, per viewer, five
minutes (a `wot_2` set up to an hour, re-validated by its `ETag`).

A Nexus behind credentials gets them from `PINAKES_NEXUS_AUTHORIZATION`, never from
`nexus_url`, which refuses user info, a query or a fragment and says so.
Its whole value is sent as the `Authorization` header (`Bearer …`, `Basic …`) on every request
to the Nexus: the feed, the reach lists and reach-set route, and the batch resource view that
the `social` tool reads. Every one goes through one client. It is sent to no other host: the
Nexus client follows no redirect, and Nominatim, sources and the egress have clients of their
own. The value is never logged or shown (errors, metrics, `/v1/info` public or admin, the
agents' texts), and the configuration's `Debug` redacts it. Empty means none: the variable set
empty clears a value the file (`nexus_authorization`) holds, unlike a source's variables, which
an empty value leaves to the file. A value that is no valid header value stops startup, without
being named.

## Queue and refresh

The durable queue leases rows with `SKIP LOCKED` and fences stale completions by a generation
counter. A job lease lasts at least `limits.lease_secs`; a 50-object OSM batch gets 760 seconds.
Failures back off exponentially from `limits.backoff_secs` to `limits.max_backoff_secs` and
dead-letter after `limits.max_failures`. The first `limits.timeout_grace` consecutive timeouts
back off without charging an attempt. A request that never got an answer from the upstream (the
connection refused, reset or aborted, or the host unreachable; stored as `connect`) is never
charged and never dead-letters: it
backs off up to `limits.max_backoff_secs` for as long as it lasts, its streak is kept in
`timeout_streak` while `last_error` is `connect`, and a link with no card yet turns `error` after
`limits.max_failures` of them in a row while it is still retried. Egress's own waits (its host
budget, a cached failure) keep that streak. A TLS failure (a certificate the host serves) and a
`connect` failure a resolver names itself (a PKARR relay, a key-pinned homeserver) are answers
about the link and count. A missing result needs a second fresh fetch to confirm,
scheduled after `limits.missing_ttl_secs` (at least an hour) for a link with an accepted card, and
after `limits.backoff_secs` for one with none yet (a new preview has nothing to keep and would show
`pending` for that hour); a cached copy cannot count twice, and the confirming fetch asks the
upstream again: what a source reads as "not there" is its own (a 404, a portal's 401 or 403, a
list without the item), so an answer the request cache keeps is not replayed to it, once per
request in that confirmation. Only a wait the upstream named is replayed, and never cut short: a
408, 429 or 5xx with its `Retry-After`, a 403 GitHub's rate limit answers. The wait is not charged as an attempt:
the row's `last_error` is `missing_unconfirmed`, it is logged (`a missing answer waits for its
confirming fetch`) and counted in `pinakes_resolution_deferred_total{reason="missing_unconfirmed"}`. A lookup or preview
moves an overdue resource to the front within its TTL, author budget and retry schedule; it
cannot revive a dead letter. Resolution is also limited per referencing author
(`limits.author_requests_per_second`); several tags never multiply fetches.

Refresh follows the time to an event's start (`limits.refresh_resource_secs`,
`limits.refresh_container_secs`): 7 days, 2 days, 6 hours and 1 hour. One fetch at end + 24
hours makes a resource final, and a new reference reopens a final resource; `geo:` stays final.
OSM places refresh after `limits.osm_ttl_secs`; a confirmed missing object stays cached as
missing for the same period. Conditional responses keep accepted facts, but validators are kept
only for calendars, where they commit with the parsed snapshot.

## Web pages and access walls

Generic web resolution reads JSON-LD, microformats, OpenGraph, oEmbed and the title, then the page
hooks of switched-on sources (WordPress and Ghost records a page names on its own origin, a media
page's object by its exact record; see [docs/sources](sources/README.md)). A hook that fails or runs
out of its three seconds leaves the page's card and logs its source at debug. A site's own JSON-LD
never names a page: its breadcrumbs, its `WebSite` node, and an organization or person whose `url`
(else `@id`) is the site's root while the page is not (the publisher, the same on every page). The
root must be this site's (the page's host, `www.` aside, or a parent domain of it): a node naming
another site's root, such as a directory page's restaurant, is about the page. A script holding only
such nodes, in an array or a `@graph` too (of any length, none with a `mainEntity`), ranks below
every other, gives the page no name, description, image, type or identity claim, and may only name
the site. Without a node about the page, the og:/twitter: tags and the title name it. Those may also
be children of `<body>` itself, where frameworks such as Next.js stream a page's metadata; a
`<meta>` inside the body's content never counts. A title loses its site suffix (the part after its
last `•`, `|`, `-`, `–`, `—` or `·`) when that part names the site as a whole word: its
`og:site_name` or the head of it, its JSON-LD name, or its host's registrable label (`fountain` in
fountain.fm, `google` in play.google.com, never a subdomain's word such as `open` or `news`). So
"TFTC: A Bitcoin Podcast • Listen on Fountain" is "TFTC: A Bitcoin Podcast". A site's root page
keeps its title. Canonical, alternate and oEmbed links, `og:url` and citation meta come from the
document head only. Media pages use the pinned 390-provider oEmbed registry (`data/`, with
SoundCloud, TikTok, Facebook and Instagram skipped; `scripts/oembed-providers` updates it):
`media.oembed = "when_needed"` (default) asks path-specific providers first and broad patterns only
for an incomplete, failed or truncated page; `"always"` also fetches every matching provider and
trusted page-advertised endpoints, which must share the page's origin or its registry provider. The
page's own metadata (its JSON-LD first) names the object and oEmbed fills what it lacks, except
where the answer comes first: from a provider whose scheme names the link exactly (it has a path, as
YouTube's `watch` and `shorts` do), or from any matching provider under `"always"`, its title,
author and thumbnail come ahead of the page's. A blank field in an answer is no value.
A card's frame ([`embed`](api.md#embed)) follows from the link alone for the players Pinakes
knows, X and Mastodon posts and OpenStreetMap cards, added when the card is served. Any other comes
from the oEmbed answer's `<iframe>` (its address only) or the page head's `twitter:player` /
HTML `og:video`, stored with the card: a registry provider is then also asked when its page is
complete but names no player of its own, for its frame alone (one oEmbed request more per read,
cached for the card's lifetime, counted as `decision="frame"`; an endpoint that refused for want
of a token is not asked so again for a day, nor is a link refused or answered without a frame). A stored frame never starts by itself: an
autoplay its address turns on is turned off. Stored cards get those frames at their next read;
`pinakesd reextract media` reads the registry providers' cards again at once.
`web.discover_feeds` (web pages) and `cards.media.discover_feeds` (media pages: podcast feed
scanning) follow RSS alternates on pages without a typed card. Discovery decisions are counted in
`pinakes_discovery_decisions_total{kind,decision}`. A podcast show's card reads the show's feed
for its latest episodes at each refresh (a feed card from the feed it is; a Podcast Index or
Apple Podcasts show page, and a player page opening a show by its Apple ID, from the feed its
record names: one feed request per read, cached for the card's lifetime, cut at 8 MiB,
fetched and parsed within three seconds and what is left of the read's deadline, so a slow feed
or a wait for a parser costs the list and never the card; a feed card whose own read was cut at
the page cap reads its feed again so for its show, keeping the cut read's card and list when that
fails or does not parse, joined by the other episodes a longer stored list holds until a read reads
the whole list again), kept in `podcast_episode` (the rows follow the document: replaced at a
save, cleared when the card is cleared or scrubbed; the newest 1000 of a feed's first 2000 items),
with whether the feed lists older ones in `podcast_episode_list` (written and cleared with the
rows), and served by
`GET /v1/resources/{id}/episodes`
([API](api.md#episodes)). A list stored before entries named their feed item names the item's
page until the show is read again.
After the upgrade that added them, the lists fill as shows are read again: `reextract all`
(feed cards are `media`, Podcast Index pages `official`, player pages `web`).

A challenge page (a bot wall, captcha or sign-in redirect) is a failed fetch: the resource turns
`unreachable` and is retried with the normal backoff, dead-lettering after
`limits.max_failures`, unless the site's own feed lists the exact link (the
`feed` wall hook, also for plain 401 and 403 answers) or a source's wall hook
names it (an IMDb title by its Wikidata item, `media_records`; [docs/sources](sources/README.md) has each source's page); then
that is the card. Its last accepted document, event dates and feed occurrences stay, and a wall never counts
toward a disappearance. Fetches that hit one count as outcome `blocked` in
`pinakes_fetch_total`.

The markers are the built-in `data/interstitial-markers.json` (text markers such as "just a
moment", "verifying your browser" or Anubis' "making sure you're not a bot", placeholder
titles, captcha selectors, wall host prefixes, sign-in paths): a wall's title never becomes a
card's name. A page on a site's own
host titled only with its name (`- YouTube`, `YouTube` on `youtube.com`, `youtu.be` and their
subdomains: placeholder titles are listed per domain), below the site's root, is the service's
stand-in for the object, a wall, at any length. Where the fetch ended decides: a short link that
redirects to such a page is a wall, and the same title on any other host is just a title. Visible text excludes `script`, `style`, `noscript` and
`template`. Title, heading and captcha markers count only on short pages (under 2,000 visible
bytes) or challenge statuses (403, 429, 503). Challenge vendors' own wording that a work could
also be named ("verifying your browser", "making sure you're not a bot", "pardon our
interruption": `challenge_text`) counts in the title or first heading on a challenge status. On
a 200 page it counts only on a short page that shows a challenge running (a vendor's challenge
script, `challenge_signatures`, or a `noscript` asking for JavaScript on a page without links, as
on MusicBrainz' proof-of-work gate) and describes no object (no `og:title`, `twitter:title`,
JSON-LD or microformat name): a short film page named so is content. An HTML answer with status 202 that is empty or
has no visible text is a challenge (AWS WAF answers bots so); an empty 202 of another type is
content. A redirect of the requested path to a listed
sign-in path (`/login`, `/authwall`, `/signin`, `/checkpoint`, …) is a wall unless the
requested path is itself one. A meta refresh without a URL is content; a refresh slower than ten
seconds is a hop only on a short page that does not refresh itself.

## Calendars and events

`[event]` enables event pages (JSON-LD Event types, microformats h-event, ActivityPub Event
negotiation), `event.calendars` the calendar feeds and `event.geocoding` venue geocoding. With
calendars off, feed URLs fall back to web cards. ActivityPub is negotiated once per resolution
for an advertised alternate, an event-looking page without an extracted Event, or an Event
without a zone. An event page can follow a same-host link to its own ICS; discovered URLs share
an LRU cache (10,000 entries, 32 MiB; one-hour minimum TTL, five minutes for failures, or the
wait the answer named when shorter; a 429 that names none keeps five minutes).

Calendar GETs follow the web policy, decompress, and are capped at `limits.event_feed_body_bytes`;
lines are unfolded from that bounded body and parsed on a blocking worker under
`limits.event_parse_timeout_ms`. A malformed VEVENT is skipped with a warning; exceeding a work
or feed cap rejects the snapshot. A recurrence rule is expanded only when one of its periods
holds at most `limits.event_max_period_dates` dates (the recurrence library builds a whole period
before giving its first date); an event with a larger rule keeps DTSTART and its RDATEs, less its
EXDATEs, with the warning `series_rule_unexpanded`. After an upgrade that changes how rules are read, run
`pinakesd reextract event`: feeds re-parse at once instead of at their next fetch. A JSON feed (Gancio, a JSON Feed) is parsed as one document,
so its parsed form can take several times `limits.event_feed_body_bytes` of memory; size that
limit with the worker count in mind. `webcal://` is fetched over HTTPS and never downgrades. A
feed-provided refresh interval is clamped to one hour through 30 days. Held snapshots retry after
`limits.held_retry_secs`, backing off from the normal cadence.

`event.utc_series_wall_clock` (default `"x_wr_timezone"`; also `"coordinates"`, `"never"`)
chooses which zone sources make a series with a UTC DTSTART expand on local wall-clock time
instead of in UTC. The default keeps Google's exports right (UTC DTSTART plus `X-WR-TIMEZONE`)
and leaves feeds whose zone comes only from coordinates in UTC. `event.default_timezone` places
floating times. A change of either applies at each calendar's next re-parse;
`pinakesd reextract event event_platform` applies it at once. Instances such a re-parse moves
get new keys once: the old rows are cancelled after two snapshots, or the feed is held if more
than half its keys move.

Page coordinates win over geocoding, and online events are not geocoded. A structured Nominatim
search is accepted only for one sufficiently matching address (`limits.geocode_score`); an OSM
venue link also needs the name and full address to match. Geocoded facts stay in `osm_place`;
documents store versioned references and compose coordinates at read time.

`event.same_event_hint` (default `"on"`; `"off"`) puts `maybe_same` on event cards: other events
that are possibly the same one ([the rule](api.md#possibly-the-same-event)). It covers the app
hint too ([possibly the same app](api.md#possibly-the-same-app)), which needs no pass: a request
compares the cards' `app_match` facts. A background pass in
the daemon computes it for events starting from a day ago to a year ahead. Triggers queue an event
resource (or a feed) when an event's start, place, name or document changes. Each pass takes up to
25 queued resources and up to 200 of their new or changed events, and the next pass follows at once
while there was work, else after five seconds. An event's normalised name, place and identifiers
are stored once read, and its pairs are matched among the events already read through their start
index, round those starting within 50 hours (an all-day listing and a timed one on one local date may start that far apart, each in its own zone). An event keeps its best twenty pairs and is named by at most forty
(`limits.same_event_incoming_kept`), and never names one whose forty all rank above it; a read
examines at most a hundred candidates, with room or not, and one that runs out with room left
goes on from where it stopped in a later read; one that changes or goes (out of the window, or
deleted as an orphan) takes its pairs from the events that named it, and those are queued to
refill their lists, as is one whose pair the caps pushed out. An hourly sweep drops events that
started over a day ago, `limits.same_event_drop_batch` (500) a transaction, and queues those that entered the window, changed
unseen, or are no longer events (an alias, a missing feed, a gone occurrence). Each change to the
pairs moves the search revision in its own transaction, so a search page's `ETag` covers the
hints it shows. Requests only read the stored pairs. `"off"` stops the pass and removes the field from
cards, details and `/v1/openapi.json`; the stored pairs stay, and changes made meanwhile are read
when it is turned back on.

### Event platforms

`data/event-adapters.json` declares each platform's mode, identity, cancellation signal, cadence
floor, expected fields, key requirement and canary. Configure a platform
under `[event.adapters.platforms.<id>]` with `enabled` and `key`; a missing key disables only
that platform and no key ever appears in `/v1/info`. Settings for `billetto_api`,
`eventfrog_api`, `google_calendar_api` or `skiddle`, which name no platform Pinakes reads, are
ignored with a warning at startup; any other unknown platform id stops startup. Resident Advisor (an unofficial GraphQL
API) is off by default; Posh is marked fragile. Billetto, Reservix, Rausgegangen and Entradium
are off and fragile: each walls its event pages off from Pinakes's user agent (a CloudFront 403,
a script challenge, an "update your browser" page), and a bot wall is never worked around.
Starticket is off: it is retired, its site redirects to See Tickets. Each platform's
`canary_note` in the table gives the reason and when it was checked. A platform that is off
sends its URLs to the web resolver, as `event.adapters.enabled = false` sends every platform's.
Every platform is read with Pinakes's own User-Agent (`/v1/info` `user_agent`), like every other
request; a platform that refuses it gets the same refused or failed result as any other page.

Luma, Meetup, Eventbrite, Partiful, Posh, RingCentral, Airmeet, Mobilizon, Gancio, Wikidata,
Dice, Xceed, Ents24 and Ticketino supplement JSON-LD with bounded state parsing; JavaScript is
never executed. A Meetup date's page (by its number or its token) also reads its own calendar
file, `{page}/ical/{id}.ics`; its UID counts only when the file holds one event and is answered
from the page's own URL origin (scheme, host and port, after redirects). That read is optional
(it is skipped on a probe): a refused, failing or non-calendar answer keeps the UID the page
was stored with, and never fails the page or marks it gone. A group calendar is read only when Meetup
answers it itself: over https, from a meetup.com host, at the calendar's path, so its `webcal:`,
`http:` and bare-domain spellings are read too; each hop of such a redirect waits for its own
slot of the host's pace. A redirect anywhere else, an HTTP one or one in the page (a meta refresh,
a script, a "Redirecting" stub), is not followed, on the platform route and on the web route
alike, whatever that answer's status or body: nothing there is fetched, the stored dates stay,
and `last_error` is `off_origin`. It is read again with the
queue's backoff (`limits.backoff_secs` doubling up to `limits.max_backoff_secs`); that streak
never counts toward the dead letter, and any other answer starts it over. After an upgrade, Meetup pages take their group calendar, their UID and
their token alias at their next read; `pinakesd reextract event_platform` reads every event
platform's links at once. A numeric Meetup page that becomes its token's alias leaves its
group, as every alias does, and an operator's curation of it (`PUT /v1/groups/{id}`) goes with
it: curate the token page again after its first read. Self-hosted Pretix, Eventyay, Gathio, Gancio and Mobilizon are detected from page
signatures, a heuristic that can cost one bounded discovery request. Authenticated APIs never
redirect, and credentials are removed from retained URLs and errors.

Cursor feeds (an event portal, BTC Map) keep their checkpoints in
`event_fetch` and commit them with each accepted batch; an expired cursor resyncs under the
ordinary hold rules. BTC Map requests every delta status and deletion, never advances its cursor
past the local fetch time, and allows five minutes of clock skew. Malformed cursor rows are
skipped (`invalid_cursor_row`, drift) and never imply deletions. With
`event.adapters.derive_resources` (default on), NIP-52 calendar members, BTC Map event
websites and the organiser calendar an event page names by its platform's rule (Meetup: a date's
page names `/{group}/events/ical/` on its own origin) are queued as resources, charged to the referring authors (or a per-adapter sponsor)
within `limits.max_calendar_members`, `limits.max_linked_resources` and
`limits.max_derived_resources`. A BTC Map link expires with its event, capped at the horizon
and extended at each feed apply as the horizon reaches it (a link that lapsed at the cap comes
back while its event is ahead). Each link records which feed rows name it: a website shared by
two events keeps the farther instant, and a tombstoned or re-linked row stops extending it;
a NIP-52 member link ends when the calendar stops listing the member and otherwise lives 24 hours
from the calendar's last fetch. An organiser calendar's link lives until the page's event ends, at
least 24 hours from the page's last fetch, capped at the horizon. A lapsed link keeps its child through the orphan grace
(`limits.grace_days`) counted from the link's expiry, so a feed that failed for a day deletes no
member, and a revived link meets the same per-parent and global caps as a new one.
Portal coordinates are page facts, never written to `osm_place`.

Nostr identifiers and client URLs use signed NIP-52 events through two HTTP indexers (Wine at one
request per second, Archives at two), paginating `until` windows within
`limits.max_indexer_pages`; the newest matching revision wins, and `nevent`/`note` pin one. Both
must miss before absence is accepted. With `event.adapters.websocket`, one relay hint is asked
after both miss (`limits.websocket_deadline_ms`, one MB, 256 frames, DNS checked and pinned); an
unfinished subscription never confirms absence. A full tied page, an exhausted budget or an
indexer 404 is inconclusive and retries after five minutes without charging an attempt.

Only parser failures fall back to a web card. Upstream failures keep the accepted document and
retry; a refresh never replaces a dated Event with a start-less card that decides nothing (a
consent wall answering 200, a date that failed to parse): the row keeps its card, records
`last_error=drift` and is read again an hour later. The one exception is a reading that decides
the page is no event at all and says so on the card (`event.not_event`: `lists_events`, a page
listing several events, read as the artist or the page itself; `creative_work`, a Wikidata item
that is a work, read as the `TVSeries` or `Book` it is, without dates): it replaces the stored
event, dates and all, as the correction it is, and a later reading that finds an event again
simply stores it ([Lists](api.md#lists)). Registry-listed secondary
denials and GraphQL failures defer, and after three days become `unreachable` with ordinary
backoff. HTTP 451 and cursor 401/403/404 follow two-observation disappearance. Direct Wikidata
404/410 do too, and a missing entity never removes a live Wikipedia article. A Wikidata item's
card shows its image only with the file's licence verified on Commons and its credit
([api.md](api.md#credits)): one Commons request per reading at most, none for an item without an
image, cached an hour. A stored card changes at its next read; `pinakesd reextract
event_platform` reads every event platform link again at once, Wikidata items among them (a
Wikipedia page read from its item is on the `web` route).

`cargo run -p pinakes-core --example adapter_probe -- MANIFEST.json REPLAYS.json OUTPUT_DIR`
(with `DATABASE_URL`) runs a read-only, keyless canary pass and records requests, the User-Agent
each sent, statuses, hashes and parser output; secrets are refused. `PINAKES_PROBE_OFFLINE=1`
replays captures without network access. Recorded responses for the keyless probes run in
`scripts/test`.

## Also on

After setting a source's key (its page in [docs/sources](sources/README.md#also-on) names
it), or after an upgrade that adds or changes also-on sources, run `pinakesd --config <path> reextract also-on`
(or `reextract all`, which includes it): until then links already stored keep the results of
their last run for up to the source's lifetime (30 days). After an upgrade that changes how links
are spelled, typed or named (aliases of a canonical link, a card's service in `site`), run
`reextract all`: stored cards keep what the older build gave them until each is fetched again.
`spotify_isrc` (with the Spotify credentials) costs one Spotify search per ISRC key of a track on
another service, and per barcode key of an album a search and up to three album reads, cached an
hour, on Spotify's pace. `tidal_isrc` (with the TIDAL credentials) costs one TIDAL request per ISRC
or barcode key of a track or album on another service, on `tidal`'s pace and token slot.
`itunes_song` and `youtube_match` (their links are maybes, `kind` `maybe`, never keys) cost
per track a Deezer, Spotify or TIDAL read and an iTunes lookup, and per label upload on YouTube a
watch page (up to 3 MB) and a Deezer and a TIDAL search; each runs its part only while the source
of that service runs (`deezer`, `spotify`, `tidal`, `tidal_isrc`, `itunes`). Every one of them is
on by default (`[also_on.sources.<id>] enabled = false` turns one off); after the upgrade that adds
them, `reextract also-on` asks for every stored track and album at once. The duplicate
check of the music sources (an ISRC MusicBrainz gives to several recordings) costs MusicBrainz
reads on a common path: for a recording MusicBrainz relates a link to, one ISRC lookup per ISRC
of the recording and, where several recordings share one, each of their records (at most 8
recordings, 16 ISRC lookups and 32 recording reads per walk), all cached; after an upgrade, `reextract all` is
several times the usual MusicBrainz traffic at its 1 request/s for a while.

The also-on worker runs every enabled source that applies to a resource ([the API](api.md#also-on))
after a save that changed its document or status, and again when the shortest lifetime among
them has passed. Its queue is `also_on_dirty`; `limits.also_on_concurrency` (2) resources run at
once, and a failed run keeps the previous rows and retries after `limits.also_on_retry_secs`
(300 s), doubling up to an hour, or after the wait a service asked for when that is longer. Each run replaces its source's rows (`resource_key`,
`derived_link`, `resource_fact`); rows live twice the source's lifetime and the hourly maintenance deletes the
expired ones (a deleted key regroups its resource). A source keeps at most
`limits.also_on_links_per_source` (32) links, `limits.also_on_keys_per_source` (16) keys and
`limits.also_on_facts_per_source` (32) facts per resource. `[also_on] enabled = false` stops the
worker; stored keys keep grouping and stored links stay on cards until they expire (up to twice
the sources' lifetimes). `derive_links = false` keeps keys and facts only: no derived links on
cards, so a tagged link an exact derivation names no longer joins by it either;
`[also_on.sources.<id>] enabled = false` turns one source off (an unknown ID stops startup). Off means no call to its API: a source
whose API has a host of its own (Deezer's `api.deezer.com`) has that host refused at the egress
while it is off, whatever path asks, and its site's pages and the links other sources give to them
stay. A source that is off (switched off, or not ready for want of what it needs) stops showing at the next start: the daemon writes the sources
that are off to `also_on_source_off`, and no card, `also_on` list, search, group or later
source reads their keys, facts or links. Those rows stay in the database and expire as before;
nothing is deleted, and a source switched on again shows its unexpired rows at once. The
resources its keys and keyed links name are regrouped in the background (`group_dirty`), and its
recordings' same-recording pairs are read again. What a source gave a card itself, written into its stored
document when the link was read (a title's name and description from `titles`, a title's or an
episode's type, an artist's type), goes with the link's next read: switching a source makes
every card holding its keys or facts due at once, so the change shows as the queue reaches them,
and a 304 never keeps it. A card records which sources' findings it carries, each with a fingerprint of every value it
wrote (`hinted_by` in its stored document: a title's name and description, a type); an unchanged
page whose card carries one that its source no longer gives as it is (the source is off, its row
expired or gone, or any of those values changed or dropped) is read again in full, without its
validators. `[also_on] enabled = false` hides nothing: it
only stops the worker. Sources run in a fixed order, and each
sees the keys the ones before it found in the same run (a barcode MusicBrainz names, then the
services looked up by it); a source that no longer applies (its key is gone) drops its rows. A
social post found gone gets no lookups, and its rows are dropped: nothing is derived from it, an
archived copy included. Its deletion takes it off the queue; `reextract` and a failed refresh put
it back on, and the worker then runs no source for it and drops its queue row. A lookup that ran
while it was found gone writes nothing, nor does one whose link became an alias meanwhile. A
source that fails or panics keeps its earlier rows and retries with backoff; the others' results
stand and the worker goes on (a panic is logged as an error). A source that asks a secondary
service besides its own (its page names which) keeps what the link and the other services gave
when that service fails: the link's own key is recorded, the failure is counted in
`pinakes_also_on_service_errors_total{service,class}`, and the run comes back after a failed
run's backoff instead of after the source's lifetime: five minutes, doubling to an hour while the
service keeps failing, or the wait the service named when longer. A run in which every service
answers resets it.
The sources run in the order [docs/sources](sources/README.md#also-on) lists them (each page says
what its source reads and costs); `[also_on] apple_storefront` (`us`, a
two-letter country code) is the storefront the iTunes lookups ask and the Apple Books and Apple
Podcasts links carry (Apple Music links carry none). `[also_on] spotify_pages` (`true`) lets
[`editions`](sources/editions.md) read a Spotify track's public page for its album where Spotify's
Web API does not answer (no credentials, or credentials it refuses); off, such Spotify links stay
unplaced (a card's own Spotify track is read for its own album either way). `[also_on.sources.musicbrainz] base_url`
(`https://musicbrainz.org/ws/2`) points every MusicBrainz read at a mirror's web service instead
(every source that reads MusicBrainz, and the cards of MusicBrainz pages), an http(s) URL without a
trailing slash and reached through the egress policy;
every instance on one public address shares MusicBrainz's 1 request/s, which a mirror does not
limit. The worker takes up to 100 due resources at a time and asks MusicBrainz about their links'
URLs together (50 to a request; MusicBrainz echoes each URL, and an answer applies only to the URL
it names) and Wikidata for their item links' items together (10 to a request). Each source's page says what it costs per link; requests that
several sources make (an item, a record) are shared through the discovery cache.

**Music enrichment.** `[web] music_enrichment` (on by default) gives a MusicBrainz artist, release
group, release or recording card (an ISRC's page with its recording's card too) an image from Wikimedia Commons and a description from
Wikipedia or Wikidata, from the entity's Wikidata item, each with its credit
([api.md](api.md#credits) says which item and which text), and a release or release group with
no free Commons file its front cover from the Cover Art Archive, by its MBID. It runs inside the card's own read,
after MusicBrainz answered, through the egress and its pace for those hosts, within
`limits.music_enrichment_timeout_ms`. At most, per card: MusicBrainz two requests more for a
recording (its works, only when it has no item of its own with an English article, and one
page of its official releases for a cover); Wikidata one
read-back of the item its own `wikidata` relation names, or, without one, one search and one
read-back for each lookup (its MBID, then for an artist up to two Discogs pages: three lookups;
hits over the 8 MB cap together are read back one by one), one read of the item's pages and
English description, and one of its image claim, and for a recording one read-back and one read
of its work's item's pages; Wikipedia two summaries
(English, then one other language), for a recording three (its item's English article, its
work's, then one other language); Commons one; the Cover Art Archive one for a release or release
group, or up to three for a recording's releases (used only when it has no free Commons file;
a release only when MusicBrainz says the archive shows its front cover), whose redirect to archive.org is kept, never followed (with
`egress.allow_hosts` set, allow `coverartarchive.org` for covers; archive.org is not needed). The
Wikimedia requests run one after another and share each host's pace (`www.wikidata.org` with the
also-on sources), so an artist found through its Discogs pages makes up to six Wikidata requests
before its text and image are asked for. The cover request runs beside them on its own host, so a
busy Wikidata does not leave an album without its cover: at the timeout only the part that has not
answered is unread (a cover found while the Commons file is unread shows only on a card with no
stored image). A `reextract web` of many such cards at once can leave some of them unread at the
timeout; they keep their stored image and description and are read again at their retry. Raising
`limits.music_enrichment_timeout_ms` (up to 13000) for that run leaves fewer unread. A failed or slow request never fails
the card: the card saves with what it found, and a field a timeout, a 429, a 5xx, Wikidata's
`maxlag` or any other unexpected status (a 403 refusing this client) left unread keeps its stored
value and credit (when this reading's MusicBrainz data with them would pass the 8 KiB document
bound, the reading gives up its last facts first, as it would for a value read now); a failed search for the item leaves
both fields so. The card is then read
again after at least `limits.music_enrichment_retry_secs` and at least twice as long as since its
last reading, up to its own refresh time (so a failure on a routine refresh waits for the next). A definite answer (no item, no
text: 404 or 410, a sitelink to a redirect, a refused file) removes the field. The egress shares concurrent requests
and keeps answers an hour (a busy Wikidata's for a second), and a wait an answer names (`Retry-After`, up to five minutes) for that
request. Stored cards change at their next read; to apply a change of this switch or of the
enrichment at once, run `pinakesd reextract web`. Switched off, the cards are MusicBrainz's alone
from their next read: what they took before goes then. MusicBrainz's own facts about the entity
(its disambiguation, genres, an artist's country and life span, an album's type and first
release) come with the entity read whether this is on or off.

**TMDB posters.** The `tmdb` source ([docs/sources](sources/README.md#also-on)) gives a `Movie` or `TVSeries` card with
no image or no description of its own TMDB's poster and overview, by the title's IMDb ID or
Wikidata item. It runs only with a TMDB credential in `TMDB_API_KEY` ([api-keys](api-keys.md#tmdb));
without one, or with `[tmdb] enabled = false`, it is switched off: no request to
`api.themoviedb.org` (the egress refuses the host), TMDB's stored rows are hidden at the next
start, every card that holds them is due at once and drops TMDB's values at that read, and
`/v1/info` lists TMDB's attribution only while a stored card still holds a TMDB value (checked
at most once a minute, over film and series cards). After setting the key, run
`pinakesd --config <path> reextract also-on` with the daemon stopped, then start it (or only
restart it and wait for each card's next also-on run, at most 30 days): the cards take their
posters at the read that follows. TMDB's rows are read again every 30 days (and after each save that
changes a card) and live 60 days from the last read that answered; a card takes TMDB's values only
from live rows, so a TMDB outage keeps them up to 60 days and the card drops them at its first
read after. The hourly maintenance brings every stored card that holds TMDB's values back to the
live rows (`also_on::refill`): a final, dead-lettered or orphaned card that is never read again
loses a value its rows would no longer write (expired, switched off, or another poster or
overview now; a change to a value the card does not show leaves it alone) there, with a change
line, its schedule untouched; it takes no new value there (only a read does). The pass reads only
`Movie` and `TVSeries` cards (by the `type_path` index), and checks a card again under its row
lock before it rewrites it. The same
pass takes out the values of a source the build no longer has, as a card's next read does. The
egress keeps TMDB's answers an hour and paces `api.themoviedb.org` at the default
`limits.host_requests_per_second` (1/s), and at no more than 4/s when that is raised (TMDB's
limit is about 40/s). Change lines hold a card's values before and after a change for
`limits.change_retention_days` (90): while a TMDB credential is set, startup refuses more than
120 days, and a change line that holds a TMDB value goes after 120 days whatever that setting says,
with TMDB on or off, so no TMDB value read more than six months ago stays served by
`POST /v1/changes/lookup`.

Sources reach their services through the egress policy and its per-host budgets, one request
at a time per host. A hub's published limit lowers its host's default pace like a source's own
(`limits.host_rates` names a host's rate outright): for the services several sources or the
resolver read, MusicBrainz and the Cover Art Archive 1 request/s each, Wikimedia (`www.wikidata.org`, `*.wikipedia.org`,
`commons.wikimedia.org`) 200/min, Open Library 3/s, iTunes about 20/min, the Wayback Machine one
request every 2 s (its CDX API asks for it; the pace covers the host, so a tagged capture's card
waits behind the lookups too); a service only one source reads, at the pace its page names. A
limit above `limits.host_requests_per_second` (1/s) leaves the default.

## Groups

Grouping runs after a document commits, from its own dirty queue with its own backoff (5–10
seconds of jitter, growing to an hour), so a grouping failure never blocks a save. A document
records its evidence as group keys: an identifier card's `doi`/`isbn` and the landing pages its
registry names (`registry`), a page's head citations (`cites_doi`, `cites_isbn`) and ICS UIDs
(`ics_uid`); the exact keys also-on sources vouch for join too (`hub:{key}`, below). Groups form
only from those (see [the API](api.md#groups)): an identifier card with
a named landing page that cites it, one UID within one feed, and curation. A group stores
membership only; cards read it when they are served. Each key of a run may
make a capped number of compatibility checks (`limits.group_max_members`² × 2, between 100,000
and 2,000,000). A key that alone exceeds the cap is parked (its unions undone) and the later
keys still run; the run settles and counts it
(`pinakes_group_budget_resources{state="parked"}`). A run stops at four caps and retries within
the hour with `last_error = 'work_budget'` (`state="stopped"`), starting at the key it stopped
at, so a component too large for one run is covered in turn; after 24 stopped runs on unchanged
input it settles with the groups the last run left until its input changes (`state="settled"`).
A join that would put two values of a single-object hub key in one group (two recordings, two
Q-ids) is refused, for the namespaces of the sources this build has and those it had (the
`single_object_namespace` table, written at start, outlives a removed source); `pinakes_group_key_conflict_resources` counts the resources whose last run
refused one, which points at a wrong ID on a hub.
Parked keys' checks count against the run. Past either limit, every set of units holding one the
run did not finish keeps each member's group, and no other set takes that group's ID; in a
stopped run so does every set that `limits.group_max_members` kept from joining another, since a
resumed run meets the keys in another order. A member deleted by orphan expiry leaves its group
at once. Writer lock contention retries after one to three seconds; curation waits up to two
seconds and answers 503 with Retry-After. `groups.cross_container_occurrences = "corroborated"`
also joins one UID across feeds when the instances are within 500 m, 300 s and a name similarity
of 0.5; a determined publisher can copy that, so it is off by default. In that mode an event page
also joins the occurrence with its own calendar file's UID in the organiser calendar its platform
names for it (a Meetup date's group calendar), within 300 s, without the place and name checks.
A page's calendar file gives its UID only when answered from the page's own URL origin (scheme,
host and port, after redirects), on every route.

`groups.same_recording_hint` (default `"on"`; `"off"`) puts `maybe_same` on `MusicRecording`
cards: other recordings that are possibly the same one ([the
rule](api.md#possibly-the-same-recording): the same normalised title and first artist, a length
within 3 s), which no key joins. A background pass in the daemon computes it: triggers queue a
recording when its name, type, status, page artist or duration changes, or an artist or duration
fact of it is read or expires; each pass takes up to 25 queued resources, and the next follows at
once while there was work, else after five seconds. A recording's normalised title and artist and
its length are stored once read, and its pairs are matched among the recordings already read. A
recording keeps its best twenty pairs and is named by at most forty, and never names one whose
forty all rank above it; a read examines at most a hundred candidates, with room or not, and one
that runs out with room left goes on from the position it stopped at in a later read; one that
changes or goes (no longer a recording, or deleted as an orphan)
takes its pairs from the recordings that named it, and those are queued to refill their lists, as
is one whose pair the caps pushed out. Each change to the pairs moves the search revision in its
own transaction, so a search page's `ETag` covers the hints it shows. An hourly sweep, which also
runs at startup, queues recordings never read (a database from before the
hint) and drops the units of resources that are no longer recordings. Requests only read the
stored pairs, leaving out the card's own group members. `"off"` stops the pass and removes the
field from recording cards (and from `/v1/openapi.json` when the event hint is off
too); the stored pairs stay.

Tenant rules (`data/multi-tenant-hosts.json`) say which part of a multi-tenant host is one
publisher; the canonical [alias](api.md#aliases) rule needs the same tenant. A rule is
`"subdomain"`, `"first_path_segment"`, `{"path_segment":N}` or
`{"path":{"segment":N,"skip":["u/*"],"extend":["s"],"query":["src"]}}`: `skip` drops a noise
window (Google Calendar's `u/{n}`), `extend` takes the next segment too (anchor.fm `/s/{id}`),
and `query` scopes by a parameter when the path is short (`embed?src=`).

## Monitoring

`GET /metrics` on the admin listener serves Prometheus series per source: fetch outcomes,
changes, canaries, the queue, ingestion lag and grouping. Admin `GET /v1/info` has the
manifests, resolver status (aliases left out: their roots answer for them), counts (`aliases`
among them), ingestion, source and canary state; public `/v1/info` shows
only the version, what the instance resolves, the limits a client must respect and the OSM
licence. Stored errors are sanitized classes (dns, connect, timeout, http_4xx, http_5xx, parse,
policy, too_large for an answer past its size cap, too_complex for a page whose parse did not
end within its limits; rate_limited for a link waiting for its host's budget). A failed resolve
is logged at warn as `resolution failed`, with the resource id (`resource_id`), the link's host
(`host`; its scheme for a link without one, never its path), this class (`kind`) and the
upstream HTTP status when one was answered (`status`), never the error's text. A source's health
(`source_state`) that cannot be written is logged at warn as `source health not recorded`, with
the source, the class and the host; the jobs and the canary still finish. No log line about
a fetch, a lookup or a parse prints an error's text (what an upstream answered may hold a link);
the lines about the daemon's own services (the database, Nexus, the dump) print theirs through
a last guard that cuts every URL before its query or fragment and drops its user info. A
`nexus_url` or `osm.nominatim_url` with user info, a query or a fragment is refused at startup. A
path is allowed, and may hold a token, so the feed's log lines (a reset, a skipped line that
panicked) name the Nexus by its host alone (`nexus`), and a Nominatim request is logged by its
path below the configured base (`path`: `lookup`, `search`); `osm.nominatim_url` itself, which may
carry a hosted Nominatim's key, shows as `[REDACTED]` in debug output and serialization. Only admin
`GET /v1/info` shows the Nexus URL, in its ingestion cursors. Logs name
a link only by its host (a calendar's token or a user's name in a link never reaches a log line),
errors carry no URL (a fetch error or a Nexus request error names none, an upstream service's
error text is not repeated, the API answers an invalid link with the reason, not the link, and
a query or body it cannot read with what the route takes, never the parameter name or value it
could not read). The MCP library logs only its warnings, whatever `RUST_LOG` asks for: at debug,
trace and info it would print whole requests, results and notifications, a tool's arguments among
them. Admin info and database metrics are cached for 15 seconds.

Canaries (`canary.enabled`) run every `limits.canary_interval_secs`, first one interval after
startup, on the ordinary workers and host budgets; their state survives restarts and they never
write documents, the OSM cache or changes. A successful result missing a declared field is
`outcome=drift`; generic web declares no fields. A canary whose host the egress policy denies
(`deny_hosts`, `allow_hosts`) is skipped before any request: its gauges are cleared, so it counts
as neither a failure nor a success. Any other refusal of a canary's request (a content type, a
redirect) is a failure. An over-budget sample is inconclusive. A platform whose canary names a
listing page (`canary_link` in the table: Infomaniak) reads that listing's first event link of
the declared form. The link is picked by its host and path alone, so the canary is only as
current as the provider keeps its listing: an expired event listed first is still the one read.
Any other platform reads its fixed sample, which its `canary_note` says when to rotate. See
[ops/README.md](../ops/README.md) for alert rules, the dashboard and scrape setup;
`scripts/check-ops` checks every alert and dashboard metric against the registry (and runs
`promtool check rules` when installed).

The capability-URL guard (`scripts/verify-no-capability-urls`, run by the tests) scans every
tracked and new source file for capability URLs; synthetic fixture URLs are allowed only in
named test files, and failures print paths without the URLs. The daemon applies the same
provider patterns, plus credential query keys and a password in the user info, at runtime
(`crates/pinakes-core/src/capability.rs`): `POST /v1/resolve` refuses such a link, and no
also-on, archive or oEmbed lookup is sent one. `crates/pinakes-core/tests/fixtures/capability_urls.json`
holds the probes the guard and the runtime check must agree on; add a pattern to both.
