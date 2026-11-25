# Operations

This is the operator's reference for `pinakesd`: every setting, what the daemon does with it, and
the procedures for running an instance. Related pages:

- [Deployment](deployment.md): what to host, sizing, containers and the production checklist.
- [API keys](api-keys.md): every credential, where to get it and what works without it.
- [Source catalogue](sources/README.md): what each source fetches, stores and credits.
- [Design](design.md): why Pinakes behaves as it does.
- [Monitoring setup](../ops/README.md): metrics, alert rules and the dashboard.

## Configuration

### Commands

| Command | What it does |
| --- | --- |
| `pinakesd [--config PATH]` | Runs the daemon. Creates the schema on an empty database. Without `--config`, only the defaults and the environment apply. |
| `pinakesd [--config PATH] --migrate` | Creates the schema on an empty database and exits. |
| `pinakesd [--config PATH] reextract ROUTE…\|all\|also-on` | Reads stored links again after a parser, routing or source change. Run it with the daemon stopped ([re-extraction](#re-extraction)). |
| `pinakesd [--config PATH] healthcheck` | Asks `GET /healthz` on the admin listener, for at most five seconds. Prints `healthy` and exits 0, or exits non-zero. |
| `pinakesd --openapi` | Prints the OpenAPI document. |
| `pinakesd --help` | Prints the usage line. |
| `pinakesd parse-worker SECS` | The HTML parse worker. The daemon starts it; you don't. |

### How values are set

A value comes from the built-in default, then the TOML file (`--config`), then the environment.
[`pinakes.example.toml`](../pinakes.example.toml) lists the operator choices and
[`.env.example`](../.env.example) the secrets. Configuration is read once, at startup: restart the
daemon to apply a change.

| Form | Use |
| --- | --- |
| `[section]` and `key` in the TOML file | Operator choices: addresses, switches, lists. |
| `[limits]` in the TOML file | Caps, timeouts, rates, backoffs and refresh times. Optional. |
| `PINAKES__SECTION__KEY` (two underscores) | Any key of the file, and it wins over the file. Strings are literal; numbers, booleans and arrays are JSON. |
| `PINAKES_NAME` and `TMDB_API_KEY` | Secrets with a variable of their own, listed below. A build without the setting ignores the variable. |

A `PINAKES__…` variable may create an entry in `event.adapters.platforms`, `sources`,
`also_on.sources` and `limits.host_rates`; elsewhere it must name an existing key. A host name in
`PINAKES__LIMITS__HOST_RATES__api.example.org` keeps its dots, which systemd `EnvironmentFile=` and
`docker run --env-file` accept but a shell can't source.

| Variable | Sets | Against a value in the file |
| --- | --- | --- |
| `PINAKES_NEXUS_AUTHORIZATION` | `nexus_authorization` | Replaces it. Set empty, it clears it. |
| `PINAKES_TIDAL_CLIENT_ID`, `PINAKES_TIDAL_CLIENT_SECRET` | `tidal.client_id`, `tidal.client_secret` | Replaces it. Empty is ignored. |
| `PINAKES_SPOTIFY_CLIENT_ID`, `PINAKES_SPOTIFY_CLIENT_SECRET` | `spotify.client_id`, `spotify.client_secret` | Replaces it. Empty is ignored. |
| `TMDB_API_KEY` | `tmdb.api_key` | Replaces it. Empty is ignored. |
| `PINAKES_GITHUB_TOKEN` | `sources.github.token` | Fills it only when unset. Empty is ignored. |
| `PINAKES_COURTLISTENER_TOKEN` | `sources.courtlistener.token` | Fills it only when unset. Empty is ignored. |
| `PINAKES_DISCOGS_CONSUMER_KEY`, `PINAKES_DISCOGS_CONSUMER_SECRET` | `also_on.sources.discogs.key`, `.secret` | Fills it only when unset. Empty is ignored. |

`RUST_LOG` sets the log filter (default `pinakes=info,pinakes_core=info,sqlx=warn`). Logs are JSON
lines on standard output.

An unknown key, an unknown section or an invalid value stops startup, with these exceptions, which
are ignored with a warning:

- `groups.cross_container_occurrences`;
- `[sources.<name>]` for a source the build doesn't have, unless it sets a `token` or `key`;
- the event platforms `billetto_api`, `eventfrog_api`, `google_calendar_api` and `skiddle`.

A startup error names the section or variable and what was expected, never the value. Secrets show
as `[REDACTED]` in debug output and never appear in `/v1/info`.

### Top-level keys

| Key | Default | Meaning |
| --- | --- | --- |
| `database_url` | `postgresql:///pinakes?host=/var/run/postgresql` | PostgreSQL connection URL. Set a URL with a password as `PINAKES__DATABASE_URL`. |
| `listen` | `127.0.0.1:8090` | Public listener. Must differ from `admin_listen`. |
| `admin_listen` | `127.0.0.1:9090` | Admin listener. |
| `admin_token` | none | Bearer token of the admin listener, at most 4096 bytes. Required unless `admin_listen` is loopback. An empty value is refused. |
| `admin_hosts` | `[]` | Host names the admin listener answers besides `localhost` and IP addresses: bare lowercase names, no scheme, port, path or wildcard. |
| `nexus_url` | `http://127.0.0.1:8080` | The Nexus whose feed is read ([ingestion](#ingestion)). No user info, query or fragment. |
| `nexus_authorization` | none | The whole `Authorization` header sent to `nexus_url`. Set it as `PINAKES_NEXUS_AUTHORIZATION`. |
| `nexus_reach_set` | `false` | Whether that Nexus serves the reach-set route. Off, `reach=wot_2` and `boost=wot_2` answer 501. |

### Resolver sections

| Key | Default | Meaning |
| --- | --- | --- |
| `web.enabled` | `true` | The web resolver. |
| `web.discover_feeds` | `false` | Follow RSS alternates (podcast feeds) on web pages without a typed card. |
| `web.music_enrichment` | `true` | Images and descriptions on MusicBrainz cards ([music enrichment](#music-enrichment)). |
| `pubky.enabled` | `true` | Other apps' `pubky://` objects ([Pubky](#pubky)). |
| `pubky.pkarr_relays` | `["https://pkarr.pubky.app", "https://pkarr.pubky.org"]` | PKARR relays: HTTPS base URLs without credentials, query or fragment. At least one while `pubky.enabled`. |
| `geo.enabled` | `true` | `geo:` URIs and map links that name a point, resolved offline. |
| `osm.enabled` | `true` | OpenStreetMap objects, read from Nominatim. |
| `osm.nominatim_url` | `https://nominatim.openstreetmap.org` | Nominatim base URL. No user info, query or fragment; a key may be in the path. |
| `osm.requests_per_second` | `0.5` | Requests per second to Nominatim, 0.001–1000. At most 1 for `openstreetmap.org`, `osm.org`, their subdomains and a public IP address. |
| `osm.dump_dir` | `dumps` | Directory of the [ODbL dump](#odbl-dump), relative to the working directory. The container image sets `/var/lib/pinakes/dumps`. |
| `cards.media.enabled`, `cards.isbn.enabled`, `cards.doi.enabled`, `cards.nostr.enabled`, `cards.activitypub.enabled` | `true` | One switch per card route. HTTP links of a switched-off route get web cards. |
| `cards.media.discover_feeds` | `false` | Scan media pages for podcast feeds. |
| `cards.official.enabled` | `true` | Every official-API source at once. |
| `media.oembed` | `"when_needed"` | `"always"` asks every matching oEmbed provider ([oEmbed](#oembed-and-frames)). |

Concurrency, rates and lifetimes are not keys of these sections; they are [limits](#limits).

### Source sections

| Key | Default | Meaning |
| --- | --- | --- |
| `sources.<name>.enabled` | `true` | One official-API source. Off, its links get web cards ([official sources](#official-sources)). |
| `sources.github.token`, `sources.courtlistener.token` | none | The source is off without its token. At most 256 printable characters. |
| `sources.stack_exchange.key` | none | Stack Exchange app key. |
| `podcast_index.key`, `podcast_index.secret` | none | Podcast Index API pair. |
| `tidal.client_id`, `tidal.client_secret` | none | TIDAL app credentials. |
| `tidal.country` | `"US"` | TIDAL catalogue country: an ISO 3166-1 alpha-2 code in upper case. |
| `spotify.client_id`, `spotify.client_secret` | none | Spotify app credentials, shared by `spotify` and `spotify_isrc`. |
| `tmdb.enabled` | `true` | Off, TMDB is off even with a credential ([TMDB](#tmdb-posters)). |
| `tmdb.api_key` | none | TMDB API Read Access Token, or v3 API key. |
| `also_on.enabled` | `true` | The [also-on](#also-on) worker. |
| `also_on.derive_links` | `true` | Off, sources keep keys and facts only. |
| `also_on.apple_storefront` | `"us"` | Two-letter lowercase storefront of the iTunes lookups and of Apple Books and Apple Podcasts links. Apple Music links carry none. |
| `also_on.spotify_pages` | `true` | Let `editions` read a Spotify track's public page where Spotify's Web API doesn't answer. |
| `also_on.sources.<id>.enabled` | `true` | One also-on source. An unknown ID stops startup. |
| `also_on.sources.musicbrainz.base_url` | `https://musicbrainz.org/ws/2` | A MusicBrainz mirror's web service ([paces](#paces-of-the-services)). No trailing slash. |
| `also_on.sources.discogs.token` | none | Discogs personal access token. |
| `also_on.sources.discogs.key`, `.secret` | none | Discogs app pair. The token wins when both are set. |

### Event and group sections

| Key | Default | Meaning |
| --- | --- | --- |
| `event.enabled` | `true` | Event pages. |
| `event.calendars` | `true` | Calendar feeds. Off, feed URLs get web cards. |
| `event.geocoding` | `true` | Venue geocoding through Nominatim. |
| `event.utc_series_wall_clock` | `"x_wr_timezone"` | Which zone sources make a UTC series follow local time: `"x_wr_timezone"`, `"coordinates"` or `"never"` ([time zones](#time-zones)). |
| `event.default_timezone` | none | Zone of floating calendar times. |
| `event.same_event_hint` | `"on"` | `maybe_same` on event and app store cards: `"on"` or `"off"` ([hint](#same-event-hint)). |
| `event.adapters.enabled` | `true` | Event platforms. Off, their links get web cards. |
| `event.adapters.derive_resources` | `true` | Queue derived links: NIP-52 members, BTC Map websites, organiser calendars. |
| `event.adapters.websocket` | `true` | Ask one Nostr relay hint when no indexer has the event. |
| `event.adapters.platforms.<id>.enabled` | per platform | One [event platform](#event-platforms). |
| `event.adapters.platforms.<id>.key` | none | The platform's API key. |
| `groups.enabled` | `true` | Automatic grouping. |
| `groups.same_recording_hint` | `"on"` | `maybe_same` on recording cards: `"on"` or `"off"` ([hint](#same-recording-hint)). |

### Network and API sections

| Key | Default | Meaning |
| --- | --- | --- |
| `http.contact` | `""` | Email address sent in the User-Agent. Empty is none. An address that names `github` is refused. |
| `egress.allow_hosts` | `[]` | Only these hosts are fetched. Empty allows every public host. |
| `egress.deny_hosts` | `[]` | Hosts never fetched. Deny wins over allow. |
| `api.requests_per_minute` | `120` | Budget of every public route per client address, 1–10000. |
| `api.trusted_proxies` | `[]` | Reverse proxies (addresses or networks) whose `X-Forwarded-For` is read ([clients](#clients-and-budgets)). |
| `api.cors_allow_origins` | `["*"]` | Browser origins allowed: `["*"]`, exact `scheme://host[:port]` origins, or `[]`. |
| `api.swagger_ui` | `true` | Swagger UI at `/swagger-ui`. |
| `api.mcp` | `true` | The MCP endpoint, `POST /mcp`. |
| `canary.enabled` | `true` | Periodic [canaries](#canaries). |

### Limits

Set a limit in `[limits]` or as `PINAKES__LIMITS__<NAME>`. A value outside its range stops startup.

#### Ingestion and refresh limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `poll_secs` | `5` | 1–3600 | Nexus feed poll interval in seconds. |
| `max_label_bytes` | `128` | 1–1024 | Longest accepted tag label (bytes). |
| `max_object_path_bytes` | `2048` | 1–2048 | Longest accepted Pubky object path (bytes). |
| `grace_days` | `7` | 0–3650 | Days an unreferenced resource is kept before orphan cleanup. |
| `preview_grace_hours` | `24` | 1–87600 | Hours a resource nothing ever referenced (one only previewed) is kept, at most `grace_days`. |
| `refresh_resource_secs` | `[604800, 172800, 21600, 3600]` | each 1–31536000 | Resource refresh intervals by [tier](#refresh-tiers) (seconds). |
| `refresh_container_secs` | `[86400, 43200, 21600, 3600]` | each 1–31536000 | Container (calendar, feed) refresh intervals by tier (seconds). |
| `refresh_cursor_secs` | `300` | 1–86400 | Cursor feed refresh interval (seconds). |
| `change_retention_days` | `90` | 1–3650 | Days change lines are retained. At most 120 while a TMDB credential is set. |
| `canary_interval_secs` | `21600` | 3600–604800 | Canary interval (seconds). |

#### Resolver limits

The web limits cover the web resolver and, each with workers of its own, the two event routes. The
resolver limits apply to pubky and to each card route.

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `web_concurrency` | `4` | 1–128 | Concurrent jobs of the web resolver, and of each event route. |
| `web_requests_per_second` | `4.0` | 0.001–1000 | Jobs per second of the web resolver, and of each event route. |
| `resolver_concurrency` | `2` | 1–32 | Concurrent jobs per pubky or card resolver. |
| `resolver_requests_per_second` | `2.0` | 0.001–100 | Jobs per second per pubky or card resolver. |
| `resolver_ttl_secs` | `86400` | 3600–31536000 | Refresh time of an official card (seconds), and how long the web and card resolvers ask their discovery answers to be kept. Other cards refresh by [tier](#refresh-tiers). |
| `pkarr_relay_timeout_ms` | `4000` | 100–7000 | How long one PKARR relay may take to answer (milliseconds), from when its request is sent ([Pubky](#pubky)). |
| `music_enrichment_timeout_ms` | `8000` | 100–13000 | How long a MusicBrainz card's enrichment may take, within the card's own read (milliseconds). Past it, the card keeps its stored image and description. |
| `music_enrichment_retry_secs` | `3600` | 60–604800 | The earliest a MusicBrainz card whose enrichment, or a Wikidata item's card whose Commons image, failed for a while is read again (seconds). It waits at least twice as long as since its last reading, up to its own refresh time. |
| `music_image_width` | `640` | 20–4096 | Width of the Commons thumbnail an enriched card or a Wikidata item's card asks for (pixels). Commons serves the next of its standard sizes (960 for 640). |
| `podcast_items` | `2000` | 1–10000 | Podcast feed items read for a show's or an episode's card. A show's episode list looks at a feed's first 2000 items at most (fewer on a feed's own card when this is lower) and keeps the newest 1000. |

#### Egress limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `host_requests_per_second` | `1.0` | 0.001–1000 | Requests per second to any one upstream host ([pacing](#pacing)). |
| `host_rates` | `{}` | each 0.001–1000 | Per-host overrides of `host_requests_per_second`. Each key is one host name (`api.example.org`; an IPv6 address in brackets), read without regard to case. A pattern (`*.example.org`), a URL, a name with a path, or two keys for one host stop startup. |
| `html_head_bytes` | `4194304` | 1–16 MiB | How far an HTML page past the 1 MiB page cap is read (bytes). |
| `hub_body_bytes` | `8388608` | 1–64 MiB | Body cap of a MusicBrainz or Wikidata answer (bytes). |
| `discovery_cache_secs` | `3600` | 3600–86400 | How long an answer the also-on sources, lookups and hubs ask for is shared (seconds). |

#### Queue limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `max_failures` | `5` | 1–32 | Charged failures before a resource dead-letters. A 5xx, 408 or 429 that a route reads as a wait charges none: such a link dead-letters only once its waits have lasted three days. A web page, calendar feed or Pubky object that answers 5xx or 408 is charged ([failures](#failures-and-retries)). |
| `timeout_grace` | `3` | 0–32 | Consecutive timeouts that charge no failure. |
| `backoff_secs` | `30` | 1–`max_backoff_secs` | First retry backoff (seconds). |
| `max_backoff_secs` | `3600` | `backoff_secs`–86400 | Retry backoff ceiling (seconds). |
| `lease_secs` | `60` | 30–3600 | Shortest job lease (seconds). |
| `missing_ttl_secs` | `3600` | 1–86400 | Re-check interval for missing resources (seconds). |
| `author_requests_per_second` | `1.0` | 0.001–1000 | Resolution budget per referencing author per second. |

#### OpenStreetMap limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `osm_ttl_secs` | `2592000` | 1–31536000 | OSM place cache lifetime (seconds). A card is scheduled no sooner than an hour. |
| `osm_dump_every_hours` | `24` | 1–168 | Hours between two writes of the ODbL dump file. |
| `osm_dump_downloads_per_hour` | `6` | 1–1000 | ODbL dump downloads per hour per client address. |

#### API limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `request_timeout_secs` | `30` | 8–600 | Seconds a request may take on either listener before it is answered 504. An MCP tool call has two seconds less for all its reads together. |
| `shutdown_grace_secs` | `30` | 1–600 | Seconds a stopping daemon gives open requests, running jobs and background passes. |
| `reach_timeout_secs` | `5` | 1–60 | Nexus reach deadline per search, and for all of an MCP `social` call's Nexus reads (seconds). Must be more than two seconds below `request_timeout_secs`. |
| `reach_cost` | `6` | 2–100 | Budget units a search costs for each of `reach` and `boost`. |
| `related_cost` | `6` | 2–100 | Budget units a related search costs. |
| `related_taggers` | `200` | 1–10000 | Taggers a related page samples: the authors of a resource's most recent tags, at most this many. |
| `related_window_days` | `365` | 1–3650 | Days a related page looks back, for those taggers and their tags. |
| `related_refs_per_tagger` | `1000` | 1–100000 | References a related page reads per sampled tagger (their most recent). |
| `related_min_shared` | `2` | 1–10000 | Shared taggers a related row needs (its group's, when collapsed). |
| `related_tag_sample` | `2000` | 1–100000 | Tags a related page reads per seed link (its most recent), from which the taggers are sampled. |
| `related_seed_links` | `32` | 1–1000 | Seed links a related page samples: of the resource, its group's members and their aliases, the ones with the newest tags, at most this many. |
| `collapse_refs_per_member` | `10000` | 1–1000000 | References counted per group member when `collapse=group` picks a group's row. |
| `max_radius_m` | `200000.0` | 1–200000 | Largest `near` radius (metres). |
| `api_cache_capacity` | `10000` | 100–100000 | Entries the rate limiter's table holds when an API request adds a client address. Past it the oldest entry is forgotten with its used budget, so keep it above the clients, preview clients and previewed links seen in a minute. Also the client addresses the dump download limiter remembers per hour. |
| `preview_cache_capacity` | `10000` | 100–100000 | The same table's size when a preview adds its client address and link. Keep it equal to `api_cache_capacity`. |
| `preview_requests_per_minute` | `30` | 1 or more | Preview requests per minute per client. |
| `preview_uri_requests_per_minute` | `30` | 1 or more | Preview requests per minute per URI. |
| `preview_new_per_hour` | `1000` | 1–1000000 | New previews per hour for the whole instance. Over it `POST /v1/resolve` answers 503. |
| `preview_new_per_client_per_hour` | `100` | 0–1000000 | New previews per hour one client address may take (0: no share). Over it `POST /v1/resolve` answers 429. |

#### Group limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `group_max_members` | `256` | 2–1000 | Most members of one automatic group. |
| `group_max_distance_m` | `500.0` | 1–10000 | Farthest two located members of one group (metres). |
| `group_max_graph_units` | `20000` | 10000–100000 | Units one grouping pass may read. |
| `group_max_graph_bytes` | `33554432` | 1–128 MiB | Bytes one grouping pass may read. |

#### Also-on limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `also_on_links_per_source` | `32` | 1–256 | Derived links one source keeps per resource. |
| `also_on_keys_per_source` | `16` | 1–64 | Join keys one source keeps per resource. |
| `also_on_facts_per_source` | `32` | 8–256 | Facts one source keeps per resource. The cap cuts the many values of one name (a recording's ISRCs) before a name's one value. |
| `also_on_concurrency` | `2` | 1–16 | Resources whose sources run at once. |
| `also_on_retry_secs` | `300` | 1–86400 | First retry of a failed source run (seconds). Doubles up to an hour. |
| `also_on_refusal_hold_secs` | `300` | 120–86400 | How long a service's refusal of the operator's credentials (TIDAL, Spotify, Podcast Index) is held before it is asked again (seconds). |
| `also_on_watch_page_bytes` | `3145728` | 1–16 MiB | Largest YouTube watch page `youtube_match` reads (bytes). A larger one is refused. |

#### Calendar and event limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `event_feed_body_bytes` | `8388608` | 1 KiB–64 MiB | Largest calendar or feed body (bytes). |
| `event_parse_timeout_ms` | `2000` | 1–5000 | Calendar parse deadline (milliseconds). |
| `event_max_line_bytes` | `65536` | 1024–262144 | Longest calendar content line (bytes). |
| `event_max_date_values` | `4096` | 1–10000 | Most date values in one calendar property. |
| `event_max_components` | `100000` | 1–100000 | Most components one calendar may hold. |
| `event_max_properties` | `1000000` | 1–10000000 | Most properties one calendar may hold, over all its components. |
| `event_max_revisions` | `10000` | 1–10000 | Most revisions of one event (RECURRENCE-ID overrides). |
| `event_max_document_bytes` | `8388608` | 1 KiB–32 MiB | Largest retained event document set per feed (bytes). |
| `event_max_recurrence_steps` | `100000` | 1–1000000 | Most recurrence steps one series may take. |
| `event_max_period_dates` | `100000` | 1–10000000 | Most dates one period of a recurrence rule may hold. A larger rule is not expanded. |
| `event_max_occurrences_per_series` | `50` | 1–1000 | Most occurrences one series expands to: its upcoming dates first, then its most recent past ones. |
| `event_max_occurrences_per_feed` | `10000` | 1–10000 | Most occurrences one feed yields. |
| `event_months_back` | `1` | 0–12 | Months of past occurrences kept in the horizon. |
| `event_months_forward` | `12` | 1–36 | Months of future occurrences kept in the horizon. |
| `geocode_score` | `0.8` | 0–1 | Lowest accepted geocoding score. |
| `held_retry_secs` | `60` | 1–604800 | Wait before a held feed snapshot's first check (seconds; at least `feed_cadence_floor`). |
| `feed_max_changes` | `100` | 1–1000 | Occurrence changes per snapshot before they become one summary line. |
| `feed_max_rows` | `10000` | 1–10000 | Most stored rows of one feed. |
| `feed_max_new_keys` | `1000` | 1–10000 | New keys one snapshot may add before it is held. |
| `feed_max_holds` | `5` | 1–32 | Consecutive holds before a feed dead-letters. |
| `feed_retention_days` | `30` | 1–3650 | Days cancelled and past rows are kept. |
| `feed_cadence_floor` | `3600` | 3600–604800 | Shortest feed refresh interval (seconds). |
| `same_event_incoming_kept` | `40` | 1–10000 | Most events that may name one event in the same-event hint (each keeps its best twenty). |
| `same_event_drop_batch` | `500` | 1–100000 | Events the same-event sweep drops per transaction. |

#### Event platform limits

| Limit | Default | Range | Meaning |
| --- | --- | --- | --- |
| `websocket_deadline_ms` | `2000` | 100–5000 | Nostr relay websocket deadline (milliseconds). |
| `max_cursor_pages` | `10` | 1–100 | Pages one cursor feed fetch may read. |
| `max_indexer_pages` | `10` | 1–20 | Pages one Nostr indexer lookup may read. |
| `max_derived_resources` | `10000` | 1–100000 | Derived links one adapter may hold across all parents. |
| `max_calendar_members` | `50` | 1–500 | Members one NIP-52 calendar may list. |
| `calendar_member_wait_hours` | `48` | 1–720 | Hours a NIP-52 calendar waits for a member that has never resolved. |
| `max_linked_resources` | `100` | 1–1000 | Website links one BTC Map feed may derive. Organiser calendars of event pages count against the same cap, per page. |

## Database

Supported: PostgreSQL 16–18 with PostGIS 3.4+ and `pg_trgm`. The daemon opens at most 20
connections for its workers and background passes and at most 10 for the API, whose statements
time out after five seconds.

### Roles and extensions

A superuser creates the database and its two extensions once:

```sql
CREATE EXTENSION postgis;
CREATE EXTENSION pg_trgm;
```

The role in `database_url` is no superuser. It owns the database (`createdb -O <role>`), or at least
has `CREATE` on its `public` schema. `--migrate`, the daemon and `reextract` all connect as that one
role. Every start checks the migration history with `CREATE TABLE IF NOT EXISTS _sqlx_migrations`,
which PostgreSQL refuses to a role without `CREATE` on the schema even when the table exists.

### Schema

`migrations/0001_schema.sql` is the whole schema. `pinakesd --migrate`, the daemon's start and
`reextract` each create it on an empty database and leave a database that already has it as it is.
A database built from another version of the file is refused and left unchanged.

Schema changes edit that file; there is no incremental migration runner and no general in-place
upgrade command. To upgrade a production database in place:

1. Back it up and restore-test the backup.
2. Write and review a procedure specific to the two versions. It must bring the schema to the
   target version's, and reconcile the `_sqlx_migrations` history (SQLx validates the checksum of
   `0001_schema.sql`) only after the schema and data change is proven. Don't delete the history or
   change its checksum just to suppress `VersionMismatch`.
3. On the restored copy, apply the procedure with the daemon stopped, and prove that the result
   matches a freshly migrated disposable database.
4. On the same copy, run the target binary through `--migrate` and a daemon start, and check its
   API and workers.
5. Apply the procedure in production, with the daemon stopped.

Don't drop a production database to upgrade it: a feed replay can't restore operator curation
(`group_override`) or the change journal (`resource_change`).

Only for a disposable local development database:

```sh
dropdb pinakes && createdb pinakes
pinakesd --config pinakes.toml --migrate
```

### Move to a new database

Where no in-place procedure exists, use a new database of the target version. The index is rebuilt
from the Nexus feed; you carry curation and the change journal over by hand.

1. Stop the daemon and back the old database up (`pg_dump`).
2. Save the curated groups from the old database, one row per member:

   ```sql
   \copy (SELECT o.group_id, u.resource_id, u.occurrence_key FROM group_override o JOIN group_unit u USING (unit_id) ORDER BY 1, 2, 3) TO 'curation.csv' CSV
   ```

3. Create the new database with its extensions and run the target binary's `pinakesd --migrate`.
4. Copy the change journal before the new daemon first starts, so the cursors clients hold stay
   valid. The two versions' `resource_change` tables must have the same columns (`\d
   resource_change` in each). Run the second command only when the first has succeeded; it loads
   the whole file or nothing.

   ```sh
   DATABASE_URL="$OLD_DATABASE_URL" scripts/pg-env pg_dump --data-only --table=resource_change --file=resource_change.sql
   DATABASE_URL="$NEW_DATABASE_URL" scripts/pg-env psql -v ON_ERROR_STOP=1 --single-transaction --file=resource_change.sql
   ```

   `scripts/pg-env` runs a command with libpq's `PG*` variables set from `DATABASE_URL`, so no URL
   or password stands on a command line. Setting `PGHOST`, `PGUSER`, `PGDATABASE` and a
   `PGPASSFILE` yourself does the same.

5. Start the daemon on the new database. It reads the feed from zero and reads every link again;
   each card gets a `created` line of its own.
6. When a curated group's members are stored and grouped again, put the group back with the admin
   `PUT /v1/groups/{id}` ([API](api.md#groups)). A resource keeps its ID as long as both versions
   normalize its URI alike. A member is `{"resource_id": …, "occurrence": null}` for an empty
   `occurrence_key`, and `{"resource_id": …, "occurrence": {"uid": …, "recurrence_id": …}}` for a
   key `["<uid>","<recurrence id>"]` (no `recurrence_id` when the second is empty).

### Instance lock

One daemon runs per database. The daemon holds a database advisory lock on a connection of its own
for its whole life, and a second daemon or a `reextract` on that database refuses to start (`another
pinakesd already owns this database`). Every daemon version takes the same key. Host, author and
client budgets are process-local, and the lock keeps their guarantees.

The daemon checks the lock's session every ten seconds and gives each check ten seconds to answer.

| Check result | What the daemon does |
| --- | --- |
| The session ended: the connection failed, PostgreSQL restarted or shut down, an administrator ended it | Stops at once. |
| Two checks in a row with no answer in time, or with an error that leaves the session open | Stops. One such check alone stops nothing. |

A daemon that has lost the lock claims no more work and gives what runs two seconds at most (less
when its shutdown grace ends sooner). Its workers and background passes therefore run without the
lock for 32 seconds at most. The process ends once its database connections are closed, up to five
seconds more, in which a request still open may be answered.

### Stop and restart

On SIGTERM or SIGINT the daemon stops taking work and gives open requests, running jobs and
background passes up to `limits.shutdown_grace_secs`. It then closes its database connections (up to
five seconds) and exits 0, or non-zero when a task panicked while stopping. What still runs after
the grace is dropped. Give the supervisor's stop timeout (`TimeoutStopSec`, `stop_grace_period`) more
than the two together.

A job cut off before its save is read again when its lease runs out. A job cut off after its save
keeps the card, and the steps after the save (the alias its page names, an episode's pages) wait for
its next full read. A read the site answers 304 doesn't run them. A card the save made final is read
again only when a new reference reopens it, or when `pinakesd reextract <route>` reads it in full.

The daemon stops by itself, with a non-zero status, when:

- it loses the instance lock;
- ingestion, grouping, maintenance, the ODbL dump, a queue worker or a listener ends or panics;
- the also-on worker or a hint pass panics (these end by themselves when switched off).

Run it under a supervisor that starts it again (`Restart=on-failure`, a container restart policy). A
signal during startup, `--migrate` or `reextract` stops them too; `--migrate` and `reextract` then
exit non-zero.

### Re-extraction

Stored cards don't change when a parser, a routing rule or an alias rule does. After an upgrade that
changes one, stop the daemon and run:

```sh
pinakesd --config <path> reextract <route…|all|also-on>
```

| Argument | Selects |
| --- | --- |
| `web`, `pubky`, `geo`, `osm`, `event`, `event_platform` | Links stored on that route, or classified to it. |
| `media`, `isbn`, `doi`, `nostr`, `activitypub`, `official` | Links of that card kind, the same way. |
| `all` | Every link. |
| `also-on` | Every resource's also-on lookups, without reading any card. It can be combined with routes. |

For the selected links, in batches of 1000, `reextract`:

- re-runs the classification and queues each link to be read at once, revived from a dead letter
  (`dead_letter=false`, `attempts=0`, `timeout_streak=0`, `last_error=NULL`) or a final state;
- drops their HTTP validators and, on the OSM route, expires their `osm_place` cache entry, so the
  next fetch reads them in full;
- queues their [also-on](#also-on) lookups, since an unchanged card wouldn't run them;
- moves a link to its alias root when a rule now gives it one, and moves an alias to the root a
  rule now names;
- releases an alias that no rule names any more: it answers for itself again and is queued on its
  route. Every release comes before any move.

An alias made from a page's canonical link (basis `canonical`) or an official API's answer (basis
`source`) isn't a rule of its URI and is never released. Neither is the alias of a link whose source
is switched off. Other aliases aren't queued; one that still holds a failure loses it, since its root
answers for it. A link entering or leaving the OSM route, or an OSM-host URL moving to web, becomes
pending.

Spellings of one link can be stored on different routes. Spotify's `open.spotify.com/embed/…` is on
`media` (`web` with `[cards.media] enabled = false`) and `embed.spotify.com` on `web`, so folding
both takes `reextract web media`, or `all`.

Every run first scrubs what stored rows still keep of a social post that is gone or whose author
opted out, and prints `scrubbed N posts gone or opted out`, then `re-queued N resources`:

| Post | What the scrub does |
| --- | --- |
| Gone (its stored card says `missing`), also when its author had opted out | The card keeps only its link, source, status, times and the type `SocialMediaPosting`. Its change lines and also-on results are deleted, and it is regrouped. |
| Opted out | Its change lines lose their documents. |
| An alias of either | Keeps only its `aliased` line. |

### Dead letters

A dead letter is read again when someone references its link anew (a new tag, reply, embed or
collection item, through any of its spellings). The reference must be newer than the card's last
read, or than the link's first storing when it was never read, so replaying the feed revives only
links referenced since. Time, a lookup and a preview never revive one.

To retry one by hand:

```sql
UPDATE resource SET dead_letter=false, attempts=0, timeout_streak=0, last_error=NULL,
  next_attempt_at=now(), lease_until=NULL, generation=generation+1 WHERE id='<resource_id>';
```

To retry a whole route's, run `pinakesd reextract <route>`, which reads the route's other links again
too.

### Rebuild groups and hints

The daemon stores its grouping policy (`groups.enabled` and the group limits) and queues every
membership again when it changes, so a changed setting takes effect as that queue drains. To rebuild
by hand:

```sql
DELETE FROM group_input;
INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;
```

To recompute every same-event pair (cards keep their current pairs until each event is read again):

```sql
UPDATE same_event_unit SET fingerprint='';
INSERT INTO same_event_dirty(resource_id) SELECT DISTINCT resource_id FROM same_event_unit ON CONFLICT DO NOTHING;
```

To recompute every same-recording pair, for example after a change of the title normalizer
(`matching.rs`):

```sql
UPDATE same_recording_unit SET fingerprint='';
INSERT INTO same_recording_dirty(resource_id) SELECT resource_id FROM same_recording_unit ON CONFLICT DO NOTHING;
```

### Maintenance

| Every | What runs |
| --- | --- |
| Minute | The search revision's rows are folded. |
| Hour | Change lines past `limits.change_retention_days`, expired occurrences and unreferenced OSM versions are deleted; then orphans ([cleanup](#cleanup)), cached OSM objects nothing refers to, expired also-on rows; then cards lose values their sources' live facts don't give ([TMDB](#tmdb-posters)). |

## Deployment and egress

For container commands, configuration and secret injection and the persistent dump mount, see
[container deployment](deployment.md#container).

### Egress policy

Every fetch a reference can steer goes through one client, the egress. Two clients are outside it:
[Nominatim and Nexus](#nominatim-and-nexus).

| Rule | Behavior |
| --- | --- |
| Schemes | `http` and `https` without credentials in the URL. For a `nostr:` link also one `wss` connection, under the same address rules. |
| Addresses | Every DNS answer is checked and the checked addresses are pinned. A host with any private, loopback, link-local, metadata or otherwise non-public address is refused. |
| Ports | The Fetch standard's bad ports (25, 22, 6667, …) are refused. |
| Hosts never fetched | `openstreetmap.org`, `osm.org` and their subdomains, Amazon, and the API hosts of [switched-off also-on sources](#switch-a-source-off). |
| Redirects | At most three, each checked again. HTML meta refresh and literal JavaScript redirects count. |
| Requests with credentials, a body or extra headers | A redirect is refused, or kept without being followed. |
| Proxies | Environment proxies are ignored. |
| Time | Ten seconds per fetch, its wait for a [pacing slot](#pacing) included. |
| Content types | HTML, JSON, JPEG, PNG, WebP, GIF, AVIF and PDF; feeds and calendars. A reader may admit more. |

Card images stay origin URLs and are never fetched. A link that is itself an image resolves to an
ImageObject card.

`egress.allow_hosts` and `egress.deny_hosts` take exact names or `*.example.org`, which covers
`example.org` too. They never override the IP policy, and deny wins. A rule may be written in Unicode
or punycode, in any case and with trailing dots (`faß.de` and `xn--fa-hia.de` are one rule); an IP
address is read as a URL reads it. A rule that names no host stops startup, and the error names the
rule.

### Body caps

| Body | Cap | Past it |
| --- | --- | --- |
| HTML page | 1 MiB, then read on to `limits.html_head_bytes` | Cut, with a `truncated` warning |
| Image or PDF | 1 MiB | Cut; the card comes from its URL |
| RSS, Atom or other XML feed read as a page | 1 MiB (8 MiB for a podcast show's episodes) | Cut after the last whole item |
| Calendar or feed on the event route | `limits.event_feed_body_bytes` | Refused |
| MusicBrainz or Wikidata answer | `limits.hub_body_bytes` | Refused, and remembered for five minutes |
| PKARR relay answer | 8 KB | Refused |
| Other | 1 MiB | Refused (`too_large`) |

A read that cuts stops as soon as it reaches its cap, so a sender that stalls there costs no timeout.

When a MusicBrainz or Wikidata answer is over its cap, the also-on sources record only what the link's
URL proves (a MusicBrainz page its MBID or ISRC, a Wikidata item its Q-id) and don't retry it.
Wikidata items verified together that are over the cap together are read one by one, and a
MusicBrainz page's card is read from the page.

### HTML parsing

Every HTML parse runs in a child process, `pinakesd parse-worker`. A daemon that can't run one
doesn't start. All parses of one job (a page's extraction, a source's answer with many HTML
descriptions, a calendar's events) share one worker and its limits: 1 s of CPU time (10 s in a debug
build), 1 GiB of memory and a wall-clock deadline. At most as many jobs parse at once as the machine
has cores, and at least two. Text without markup (no `<`) is parsed in the daemon.

| What happens | Result |
| --- | --- |
| Past the CPU time or memory, or a tree of more than 500,000 nodes | `too_complex`: final, with no card. An earlier card is cleared and the change recorded. `pinakesd reextract` revives it. |
| Killed at the wall-clock deadline, ended from outside, or the caller stopped waiting | `timeout`: the card stays and the link is read again with the timeout backoff. |

Either way the job's later parses fail at once, and the daemon logs `an HTML parse did not end within
its limits` once per job. Before a parse, a linear scan cuts the text at the first obvious cap: 50,000
start tags, 256 open elements, 64 attributes on one tag, an attribute value of 1 MiB or a tag name of
256 bytes. A node more than 256 elements deep is left out.

### Pacing

One request at a time goes to a host, and each host has a rate. A small fixed jitter per host (at most
0.1 s) is added to its interval.

1. `limits.host_rates` names the host: that rate.
2. Otherwise `limits.host_requests_per_second` (1), or the service's [own pace](#paces-of-the-services)
   when that is lower. A source's pace never raises the default.

A request is also paced by the address it connects to, at the default rate or the host's when the
operator set that faster. Many names on one server are therefore paced together. A service's own host,
named in the code with its pace, and the PKARR relays are paced by host alone.

A request that would wait more than five seconds for its slot (or behind the request ahead of it) is
put off as `rate_limited` instead of timing out. It charges nothing. A redirect within the host keeps
the slot it has.

### Discovery cache

Discovery reads share one in-process cache (10,000 entries, 32 MiB, least recently used evicted), and
concurrent readers of one request share one fetch. They are the also-on sources and hubs, oEmbed
endpoints, feed and calendar discovery, and a few page hooks and sources (`feed`, `media_records`,
Discogs, GitHub). The page fetch of web resolution and most official-API reads don't use it.

| Answer | Kept |
| --- | --- |
| Success | The lifetime the reader asks for, at least an hour |
| Failure | Five minutes, or the wait the service named, held between 1 and 300 seconds |
| A 429 that names no wait | Five minutes |
| TIDAL's and Spotify's 401 and 403; GitHub's and TMDB's 401 | Never, so a new token asks again |
| A wait for Pinakes's own host budget | Never |

### User-Agent and contact

Every request introduces itself as `pinakes/<version> (+https://pubky.app; <contact>)`, with
`http.contact` as the contact, or `pinakes/<version> (+https://pubky.app)` without one. Set the contact
to an address the services you read can reach: some services refuse requests without one, and the
links of sources that need it (SEC EDGAR filings) wait until it is set.

### Official sources

The official-API sources ([catalogue](sources/README.md)) run on the `official` card route, each with
its own switch:

```toml
[sources.zenodo]
enabled = false
```

or `PINAKES__SOURCES__ZENODO__ENABLED=false`. A switched-off source's links get ordinary web cards,
and its address rules don't merge new links. Links it merged before stay merged.

- **Tokens.** `github` and `courtlistener` are on only with their token. A token is sent only to its
  service's API and never through a redirect.
- **Refused credentials.** A refused GitHub, CourtListener, Discogs or Podcast Index credential is off
  for an hour: GitHub, CourtListener and Podcast Index links get web cards meanwhile, and Discogs reads
  without it. A refusal is a 401, or a 403 where the service uses it for a refused key; never a 429 or
  5xx. Each one counts in `pinakes_source_token_refusals_total{source}`, which the `ProviderKeyRefused`
  alert reads for every keyed provider.
- **Refresh.** An official card is read again after `limits.resolver_ttl_secs`, sooner where its source
  asks, and social content within the hour (Instagram's within the day). A link a social site refuses
  is read again after a day, doubling to a week ([failures](#failures-and-retries)). A card with a date (an earthquake, a finished trial, a
  Bitcoin block or confirmed transaction) is final a day after its start or end has passed.
- **A build with more sources.** Run `pinakesd reextract web official`, so links already known get
  their source's card.

### Nominatim and Nexus

Nominatim (`osm.nominatim_url`) and Nexus (`nexus_url`) are operator-configured upstreams at URLs no
reference can change. Their clients follow no redirect and don't apply the DNS pinning or the
private-address refusal, so protect their URLs and DNS. A Nominatim `Retry-After` is held to an hour at
most. A failed Nexus poll is retried after `limits.poll_secs`.

Staging and production use a self-hosted or hosted Nominatim
([deployment](deployment.md#nominatim)); the public one is for a small development run only. Pinakes
asks Nominatim for object lookups and structured venue searches. It never proxies place search,
autocomplete or reverse geocoding.

A Nominatim 429 is the operator's own quota and charges no link. A 401 or 403 is counted as a refused
key (`source="osm"`) and the link is read again later.

### ODbL dump

The daemon writes `osm_place.ndjson.gz` into `osm.dump_dir` at startup when it is missing or older than
`limits.osm_dump_every_hours`, then at that interval. Why the dump exists is in
[design](design.md#odbl-dump).

- **Space.** The directory must be writable by the daemon and hold two copies of the file while one is
  written.
- **Writing.** The daemon reads one repeatable-read snapshot without a statement timeout into a
  temporary file (`osm_place.ndjson.gz.{pid}.{random}.tmp`), syncs it and renames it into place.
- **One writer per directory.** A writer holds `osm_place.ndjson.gz.lock`, so daemons sharing the
  directory take turns. A writer, and the daemon at startup, remove temporary files an interrupted
  write left.
- **Failure.** A failed or refused write keeps the previous file and is retried after five minutes.
  Shutdown cancels a write in progress; one already at its rename finishes it.
- **Downloads.** They read the file only, `limits.osm_dump_downloads_per_hour` per address. `HEAD`, 304
  and 416 are free. A CDN may cache the file by its `ETag`.

The `osm_place` cache holds what Pinakes serves. The hourly maintenance deletes cached objects that no
OSM card, no venue and no change line refers to, once their cached fetch expired an hour ago or more:
at most 1000 a pass, oldest cache first.

### Pubky

Pinakes fetches a `pubky://` object through the egress policy, from the homeserver's public HTTPS
domain. Which objects are fetched and what their cards hold is per app ([Pubky apps](pubky-apps.md)).
After an upgrade that adds or changes an app's profile, or takes a namespace off those never fetched,
run `pinakesd reextract pubky`.

1. Pinakes reads the owner's `_pubky` PKARR record from every relay in `pubky.pkarr_relays` at once,
   following a delegation to the homeserver's key at most seven deep. A 404 means no record.
2. It checks every packet's signature against the owner's key and uses the newest packet that verifies.
3. It takes the first endpoint with a public HTTPS (ICANN) domain and GETs the object path there with
   the `pubky-host` header. The usual egress rules apply, and a redirect is refused.

Objects may also arrive as `application/octet-stream`, `text/plain` or without a content type. An
object is read as JSON whatever its declared type. One that isn't JSON becomes a card with only its
content type and size (no card, for a kind an app's profile reads).

**A homeserver that publishes only key-pinned (raw-public-key TLS) endpoints is unreachable**
(`connect`, with a message that says so).

Limits on a relay request:

| Part | Limit |
| --- | --- |
| Wait for the relay host's slot | 1.5 s. A longer wait sends no request and charges nothing. |
| The request, once sent | `limits.pkarr_relay_timeout_ms` |
| The whole ask, with its DNS lookup and reading the answer | 1.5 s + that limit + 0.5 s |
| Answer | 8 KB, whatever its content type |

Verified packets are cached for their smallest record TTL, between five minutes and a day (10,000 keys
at most). "No record", when every relay answers 404, is cached for five minutes. A relay can withhold a
record but can't forge one, and Pinakes never uses a packet older than the newest it remembers for that
key. That memory holds 10,000 keys and is empty after a restart.

| When no relay gave a usable packet | Result |
| --- | --- |
| Every relay answered 404 | No record: `dns`, charged, and dead-lettered after `limits.max_failures`. |
| Each relay that failed answered 5xx or didn't answer in time, and none answered 404 | `relay_outage`: read again after the longest `Retry-After` named (a minute when none was). No attempt is charged, and it never dead-letters. |
| A relay answered 429 or its host had to be waited for, and none failed otherwise | Read again when the relay says, or its host is free. No attempt is charged. |
| Every relay refused or reset the connection | Never reached an answer: retried with backoff, never dead-letters. |
| Refused by the policy, a redirect, a 4xx other than 404 and 429, or a packet that doesn't verify; a 5xx, a timeout or a refused connection beside another relay's 404; or only a packet older than one seen before | `connect`: retried with backoff, and dead-lettered after `limits.max_failures`. |

A refusal by the policy on the homeserver side is stored as `policy` and dead-lettered at the first
attempt: a redirect, a content type outside the list, a non-public address, an owner host that isn't
the canonical z32 key, a path that changes when read as an HTTPS path, or an endpoint that is an IP
address.

Homeservers and relays count against the per-host budget. Most objects live on a few homeservers, so a
busy one may need its own rate:

```toml
[limits.host_rates]
"homeserver.example" = 4.0
```

`limits.resolver_concurrency`, `limits.resolver_requests_per_second` and the author budget apply on
top.

### Clients and budgets

| Budget | Counted per | Set by |
| --- | --- | --- |
| Every public route: `/v1`, Swagger UI, the agents' guide and `/mcp` | client address, a minute | `api.requests_per_minute` |
| Previews (`POST /v1/resolve`) | client address and link, a minute | `limits.preview_requests_per_minute`, `limits.preview_uri_requests_per_minute` |
| New previews | instance and client address, an hour | `limits.preview_new_per_hour`, `limits.preview_new_per_client_per_hour` |
| ODbL dump downloads | client address, an hour | `limits.osm_dump_downloads_per_hour` |

**Client address.** The address is the connecting peer's, unless the peer is in `api.trusted_proxies`
(addresses or networks, such as `["172.17.0.1"]` for a proxy at the Docker bridge gateway). From a
trusted proxy it is the right-most `X-Forwarded-For` entry that isn't itself a trusted proxy; with no
such entry, the proxy's own address. From any other peer the header is never read. Leave the list
empty where clients connect directly, and list exactly the proxies that append their peer's address: a
proxy that passes on a client's header unchanged lets clients choose their address.

**CORS.** `api.cors_allow_origins` lets browser apps on other origins call the public listener.

| Value | Effect |
| --- | --- |
| `["*"]` (default) | Every response carries `Access-Control-Allow-Origin: *` and exposes `Retry-After` and `ETag`. |
| Exact `scheme://host[:port]` origins | Only those origins get the headers, and every response adds `Vary: Origin`. No wildcard patterns. |
| `[]` | No CORS headers. |

Preflights get 204 with `Access-Control-Allow-Methods: GET, POST`, `Access-Control-Allow-Headers:
content-type` and `Access-Control-Max-Age: 600`, and spend the per-client budget. No
`Access-Control-Allow-Credentials` is sent, and the admin listener never sends CORS headers.

**Admin listener.** Without `admin_token`, startup logs a warning: any process on the machine can use
the listener. Terminate TLS at a trusted reverse proxy for any other bind.

| Route | What it does |
| --- | --- |
| `GET /healthz` | 200 when the database answers within a second and no background task has ended, else 503. The one route that takes no token. |
| `GET /metrics` | Prometheus metrics. |
| `GET /v1/info` | Operator info. |
| `GET /v1/resources/{id}/document` | The stored document. |
| `PUT /v1/groups/{id}`, `DELETE /v1/groups/{id}` | Curate a group, and undo it. |

The listener answers a request only when its `Host` (or HTTP/2 authority) is `localhost`, an IP address
(any port) or a name in `admin_hosts`. Any other gets 421 `admin host not allowed`, before the token is
checked; this keeps out a web page that rebinds its own DNS name to this machine. A request with no
`Host` passes. A Prometheus in another container that scrapes `pinakes:9090` needs
`admin_hosts = ["pinakes"]`.

**Swagger UI.** `api.swagger_ui` serves the vendored `utoipa-swagger-ui` bundle (Swagger UI 5.17.14) at
`/swagger-ui`. Builds never download it; a build environment must not set `SWAGGER_UI_DOWNLOAD_URL`.
Its responses carry `X-Content-Type-Options: nosniff` and a CSP without inline script.

**Agents.** The agents' guide (`/llms.txt`, `/skill.md`, the index at `/`) and the MCP endpoint
(`POST /mcp`) are on the public listener ([API](api.md#agents)). The MCP endpoint is stateless, takes no
credentials, accepts any `Host` and writes nothing but the previews that `card` with `resolve: true`
asks for. The reverse proxy must pass the public `Host` (nginx: `proxy_set_header Host $host;`) and
`X-Forwarded-Proto`, since the guide names its base URL from them. Turning `api.mcp` off removes `/mcp`
and its link from the index; the guide stays.

## Ingestion

`pinakesd` polls the Nexus reference feed (`GET /v1/resources/changes`, 1000 lines a page) every
`limits.poll_secs`, and at once while a page held lines. A Nexus request may take ten seconds and answer
8 MiB. During a Nexus outage cached data stays served and polling goes on.

### Feed lines

Pinakes keys each line on its `uri`, normalized and hashed into its own ID. Nexus's `resource_id` isn't
read. A page is parsed line by line; only its envelope (`lines`, `next_cursor`) must be well formed.

A line Pinakes can't use is skipped. After the batch commits it is counted in
`pinakes_ingest_rejected_total{reason}`, and the cursor moves past it. No log shows its content.

| `reason` | The line |
| --- | --- |
| `unknown_kind` | Is of a kind this build doesn't read. A `kind:event` line, which Nexus reserves, is one. |
| `malformed` | Can't be parsed. |
| `author` | Has an author that isn't 52 lowercase z-base-32 characters. |
| `label` | Has a label that is empty, holds a NUL or exceeds `limits.max_label_bytes`. |
| `object_path` | Has an object path that is empty, holds a NUL or exceeds `limits.max_object_path_bytes`; or, for a post, isn't a version file directly in the post. |
| `timestamp` | Has a time outside years 1–9999. |
| `uri` | Has a URI that doesn't normalize, or that still holds a control character once normalized; or, for a post, isn't a post in its author's own storage. |
| `never_fetched` | Is a post in a namespace Pinakes never fetches ([posts](#posts)). |
| `collection_items` | Is a new item of a collection post that already holds 100. |
| `panic` | Panicked while it was processed. It is rolled back alone; the log line names its feed position. |

A panic anywhere else in a poll rolls the batch back, holds the cursor and counts in
`pinakes_ingest_panics_total`. The poll is retried.

References are keyed as Nexus keys them, so a repeated put or del changes nothing more:

| Kind | Key |
| --- | --- |
| Tag | Author, `uri` and label. A del that names another tag file than the put deletes the same tag. |
| Reply, embed, collection item | Author, `object_path`, kind and `uri`. |
| Post | Its version file. |

### Feed resets

A changed feed epoch, or a cursor beyond the feed's length, means that the feed started over. The first
such answer writes nothing and logs a warning. Only the next read that reports the same epoch and still
starts over, at least `limits.poll_secs` later and from the same cursor, clears the references and
scores and restarts at zero. An answer in between that doesn't start over cancels it. After a reset the
next poll waits `limits.poll_secs`, and the cards stay through the grace period.

### Cleanup

The hourly cleanup deletes orphans in batches of 100, until it has caught up or for two minutes at most.

| Resource | Kept for |
| --- | --- |
| Lost its last reference | `limits.grace_days` after that |
| Never referenced (only previewed) | `limits.preview_grace_hours`, at most `limits.grace_days` |
| A derived child | `limits.grace_days` after its link's expiry, however it was created |
| A venue of a card or an occurrence | As long as something names it |

A child or venue whose holder is deleted keeps the full grace counted from its creation, never the
preview grace.

### New previews

`POST /v1/resolve` stores at most `limits.preview_new_per_hour` links the instance didn't know in any
hour. A known link still reads.

| Over | Answer | Counted as |
| --- | --- | --- |
| The per-client budget | 429, before the two below | |
| One client's `limits.preview_new_per_client_per_hour` (0 is no share) | 429 with `Retry-After` | `pinakes_preview_refused_total{limit="client"}` |
| The instance's `limits.preview_new_per_hour` | 503 with `Retry-After`; nothing is stored | `pinakes_preview_refused_total{limit="instance"}` |

A place is kept only by the request that stored the link; a request that fails or goes away first gives
its place back. The hour's window is in memory and starts empty. The `PreviewCapacityReached` alert
fires on either refusal. `pinakes_preview_backlog` counts the resources nothing ever referenced.

Behind a reverse proxy, list it in `api.trusted_proxies`. Otherwise every client has the proxy's
address and one share caps them all ([clients and budgets](#clients-and-budgets)).

### Nexus credentials

A Nexus behind credentials gets them from `PINAKES_NEXUS_AUTHORIZATION`, whole, as the `Authorization`
header (`Bearer …`, `Basic …`) of every request to the Nexus: the feed, the reach reads and the batch
resource view that the MCP `social` tool reads. It is sent to no other host. The value is never logged or
shown. One that isn't a valid header value stops startup, without being named. The daemon warns at
startup when it would cross the network in clear: `nexus_url` is `http://` and its host isn't loopback,
private or link-local as written.

### Reach

Search's `reach` and `boost` read the viewer's `following` and `friends` lists from the same Nexus.
`nexus_reach_set = true` says that the Nexus also serves the reach-set route
(`GET /v1/user/{viewer}/reach-set?depth=2`), which `wot_2` reads. While it is `false`, `wot_2` answers
501 and the Nexus isn't asked. Sets are kept in memory only, per viewer, for five minutes. A `wot_2` set
is kept up to an hour and revalidated by its `ETag`.

### Changing nexus_url

The references are the projection of one Nexus feed, and its cursor is kept under `nexus_url` without a
trailing `/`. The daemon refuses to start on a database that holds the cursor of another Nexus and none
of the configured one. The error names the other feed's host and this section.

Before you change `nexus_url`, stop the daemon and do one of these:

- **Another feed:** clear the projection. The next start reads the configured feed from zero.

  ```sql
  UPDATE resource SET orphaned_at=coalesce(orphaned_at,now()); TRUNCATE reference, resource_score, feed_cursor;
  ```

- **The same feed at a new address:** move its cursor row, so the feed is read on from where it was.

  ```sql
  UPDATE feed_cursor SET nexus='<new nexus_url>';
  ```

A database with a cursor row for the configured URL starts. Another feed's row left beside it is logged
by host at each start and still counts in `pinakes_ingest_lag_lines`; delete it:

```sql
DELETE FROM feed_cursor WHERE nexus='<old nexus_url>';
```

A row under another spelling of the configured URL is the same feed. The daemon moves it to the
configured key at start, or drops it beside a row already there.

### Posts

Apps' posts come on the same feed as `post` lines. A line's `uri` is the post itself, versionless
(`pubky://<author>/pub/<namespace>/v1/posts/{id}`), and its `object_path` is the version file written
(`…/{id}/{editId}.json`).

- A post line is a reference of the post to itself. It keeps the post's card alive and counts nowhere
  (scores, trending, reference filters).
- A new version, or a deleted one, makes the card due at once whatever its TTL. The Pubky resolver lists
  the post's folder newest first and reads the latest version by its namespace and kind
  ([Pubky apps](pubky-apps.md#posts)). A post without a version left is `missing`.
- A post in a namespace Pinakes never fetches (`social` and `pubky.app`, read decoded, so `%73ocial` is
  `social`) is rejected: no resource, no reference, no fetch. Social events and RSVPs are indexed by
  Nexus. Letter case is kept, so `Social` is another app's namespace.
- A namespace that isn't one path segment once decoded (not UTF-8, an encoded `/`, `.` or `..`) is
  rejected as `uri`.

## Queue and refresh

Each link is a row of a durable queue. Why refresh works this way is in [design](design.md#refresh).

### Order and leases

A worker takes rows in this order: those a lookup or a preview moved to the front, links never read,
overdue refreshes by how long they have been due. A derived link nothing keeps isn't taken.

A job's lease lasts at least `limits.lease_secs`, and 15 seconds per job of its batch plus 10; a
50-object OSM batch gets 760 seconds. One link's read may take 15 seconds.

A lookup or preview moves an overdue resource to the front within its TTL, author budget and retry
schedule. It can't revive a dead letter; a new reference does ([dead letters](#dead-letters)).

Resolution is limited per referencing author (`limits.author_requests_per_second`), so several tags of
one link never multiply fetches. When an author is over budget, the other due links only that author
referenced are put off too, each to its turn, and the queue goes on with other authors' links. `geo:`
links have no budget.

The route workers are:

| Route | Workers | Jobs per second |
| --- | --- | --- |
| `web`, `event`, `event_platform` (each) | `limits.web_concurrency` | `limits.web_requests_per_second` |
| `pubky` and each card route | `limits.resolver_concurrency` | `limits.resolver_requests_per_second` |
| `osm` | 1 | `osm.requests_per_second` (up to 50 objects a batch) |
| `geo` | 1 | 100 |

### Failures and retries

Failures back off exponentially from `limits.backoff_secs` to `limits.max_backoff_secs`. The stored
class (`last_error`) decides what a failure costs:

| Class | When | Charged | Dead-letters |
| --- | --- | --- | --- |
| `timeout` | No answer in time | Past `limits.timeout_grace` in a row (by default from the fourth) | After `limits.max_failures` charged |
| `upstream`, `http_5xx`, `http_4xx` as a wait | An official source, an ISBN or DOI card, or an event platform got a 5xx, 408 or 429; Nominatim answered 5xx; any other request got a 429 | No | Only once the failures have lasted three days |
| `connect`, never answered | Connection refused, reset or aborted, or host unreachable | No | Never |
| `connect`, named by a resolver; TLS failures (stored `connect`) | A PKARR relay, a key-pinned homeserver, a bad certificate | Yes | After `limits.max_failures` |
| `rate_limited`, `discovery_cached`, `quota` | The daemon's own wait: its host budget, a cached failure, the operator's quota (GitHub's token limit, Nominatim's 429) | No | Never |
| `upstream_denied`, `graphql` | A secondary request was denied, or a GraphQL error | No, for three days | Never. After three days the link is `unreachable`, with ordinary backoff. |
| `off_origin` | An organiser calendar answered from another origin | No | Never |
| `drift` | A refresh lost an event's start | No; read again in an hour | Never |
| `relay_outage`, `inconclusive`, `indexer_unavailable`, `disabled` and other waits a source names | PKARR relays are down; Nostr indexers couldn't settle; a setting is missing (SEC EDGAR without a contact) | No | Never |
| `missing_unconfirmed` | One missing answer, not yet confirmed | No | Never |
| `wall` | A [challenge page](#access-walls); status `unreachable` | Yes | After `limits.max_failures` |
| `wall` on a social card | A social site refused a read: a stored card that says more stays with its status, else the link gets the `blocked` card | No; read again after 1, 2, 4, then every 7 days | Never |
| `policy` | Refused by the egress policy | Yes | At the first attempt |
| `unsupported` | An OSM-host page that is no object or location link | Final | No |
| `too_complex` | A [parse](#html-parsing) past its limits | Final | No |
| `http_5xx`, `http_4xx` | A web page, calendar feed or Pubky object answered 5xx, or a 4xx that is neither missing, unreachable nor a 429 (408 included) | Yes | After `limits.max_failures`: at the defaults, about 8 minutes (30 + 60 + 120 + 240 seconds of backoff) |
| Any charged failure on the `geo` route | A `geo:` URI or map point that fails to resolve | Yes | At the first attempt |
| `dns`, `parse`, `too_large` and the rest | Anything else | Yes | After `limits.max_failures` |

Details:

- **Upstream waits.** The link is read again after the wait the service named or the backoff, whichever
  is longer. The backoff doubles while one of these answers follows another (the streak is kept in
  `timeout_streak`). Three days are measured as the backoff of the streak's waits from its first; at the
  defaults that is the 79th in a row. Until then an accepted card keeps its status, and a link with no
  card yet shows `error` after `limits.max_failures` in a row.
- **Never answered.** The streak is kept in `timeout_streak` while `last_error` is `connect`. A link
  with no card yet turns `error` after `limits.max_failures` in a row and is still retried.
- **Refusals.** A social site's refusal (its sign-in redirect, a challenge page, a 401, 403 or 451) is
  read again a day later, then 2, 4 and 7 days later, and every 7 days while the refusals go on. The
  streak is kept in `timeout_streak` while `last_error` is `wall`, both for a kept card and for a
  `blocked` card; a read that answers ends it. A card found gone stays gone: a refusal says nothing
  about the object. Asking a site that refuses the request again soon only makes it refuse longer.
- **Own waits** keep the streak of a refused connection, an upstream wait or a refusal.

### Missing results

A missing result needs a second fresh fetch to confirm it.

| Link | Confirming fetch after |
| --- | --- |
| With an accepted card | `limits.missing_ttl_secs`, at least an hour |
| Without one yet | `limits.backoff_secs` |

The confirming fetch asks the upstream again, once per request, instead of replaying a cached answer.
Only a wait is replayed: an answer that names one (a 408, 429 or 5xx, or GitHub's 403 for its rate
limit), or a cached failure with no answer, such as a transport error. The
wait charges nothing: `last_error` is `missing_unconfirmed`, the log line is `a missing answer waits for
its confirming fetch`, and `pinakes_resolution_deferred_total{reason="missing_unconfirmed"}` counts it.

A social page answered without its tags, on a stored card that says more than the link's plain card,
waits and is counted the same way (the log line carries `shapeless`). Its confirming read stores the
plain card.

### Refresh tiers

| Time to the event's start | `refresh_resource_secs` | `refresh_container_secs` |
| --- | --- | --- |
| No date, or more than 30 days | 7 days | 1 day |
| 7 to 30 days | 2 days | 12 hours |
| 1 to 7 days | 6 hours | 6 hours |
| Under a day, or started and not ended | 1 hour | 1 hour |

A feed's own refresh interval, held between an hour and 30 days, replaces the first two container tiers,
and the third when shorter. A feed follows its nearest date. Cursor feeds use
`limits.refresh_cursor_secs` or the platform's cadence floor, whichever is longer.

| Card | Refresh |
| --- | --- |
| Web, media and other undated cards | The first tier |
| Official cards | `limits.resolver_ttl_secs`, or sooner where the source asks |
| Social content | Within the hour; Instagram's within the day. While the site refuses the read, the [refusals' backoff](#failures-and-retries) |
| A dated card | One fetch 24 hours after its end (or start) makes it final |
| A job posting | Its posting date doesn't end its refreshes |
| OSM places | `limits.osm_ttl_secs`. A confirmed missing object stays cached as missing for as long. |
| `geo:` and map points | Final |
| A card that changes by itself at a known time (an auction ends) | No later than then |

A new reference reopens a final resource; `geo:` stays final. HTTP validators are kept only for
calendars, where they commit with the parsed snapshot.

## Web pages and access walls

Why a wall keeps the last card is in [design](design.md#access-walls).

### What names a page

Generic web resolution reads JSON-LD, microformats, OpenGraph, oEmbed and the title. The page hooks of
switched-on sources follow (WordPress and Ghost records a page names on its own origin, a media page's
object by its exact record; see the [source catalogue](sources/README.md)). A hook that fails or runs out
of its three seconds leaves the page's card and logs its source at debug.

- **A site's own JSON-LD never names a page:** its breadcrumbs, its `WebSite` node, and an organization
  or person whose `url` (else `@id`) is the root of this site while the page isn't. A node naming another
  site's root (a directory page's restaurant) is about the page. A script that holds only such nodes ranks
  below every other and may only name the site.
- **Without a node about the page**, the `og:` and `twitter:` tags and the title name it. Those tags may
  also be children of `<body>` itself; a `<meta>` inside the body's content never counts.
- **A title loses its site suffix**, the part after its last `•`, `|`, `-`, `–`, `—` or `·`, when that part
  names the site as a whole word: its `og:site_name` or the head of it, its JSON-LD name, or its host's
  registrable label (`fountain` in fountain.fm). A site's root page keeps its title.
- **Canonical, alternate and oEmbed links, `og:url` and citation meta** come from the document head only.

### oEmbed and frames

Media pages use the pinned 390-provider oEmbed registry (`data/oembed-providers.json`; SoundCloud,
TikTok, Facebook and Instagram are skipped). `scripts/oembed-providers` updates it.

| `media.oembed` | Asks |
| --- | --- |
| `"when_needed"` (default) | Path-specific providers first; broad patterns only for an incomplete, failed or truncated page. |
| `"always"` | Every matching provider too, and endpoints the page advertises on its own origin or its registry provider's. |

The page's own metadata names the object and oEmbed fills what it lacks. A provider whose scheme names
the link exactly (with a path, as YouTube's `watch` and `shorts`), or any matching provider under
`"always"`, puts its title, author and thumbnail ahead of the page's.

A card's frame ([`embed`](api.md#embed)) follows from the link alone for the players Pinakes knows, X and
Mastodon posts and OpenStreetMap cards, added when the card is served. Any other comes from the oEmbed
answer's `<iframe>` (its address only) or the page head's `twitter:player` or HTML `og:video`, and is
stored with the card. A registry provider is asked for its frame alone when its page is complete but
names no player: one more request per read, counted as `decision="frame"`. An endpoint that refused for
want of a token, and a link refused or answered without a frame, isn't asked again for a day. A stored
frame never starts by itself.

`pinakesd reextract media` reads the registry providers' cards again at once. Discovery decisions are
counted in `pinakes_discovery_decisions_total{kind,decision}`, with kinds `oembed`, `feed`, `page_hook`
and `wall_hook`.

### Podcast shows

A show's card reads the show's feed for its latest episodes at each refresh: a feed card the feed it is,
a Podcast Index or Apple Podcasts show page and a player page opening a show by its Apple ID the feed its
record names. It is one feed request per read, cut at 8 MiB and held to three seconds. A slow feed costs
the list and never the card.

Episodes are kept in `podcast_episode` (the newest 1000 of a feed's first 2000 items), and whether the
feed lists older ones in `podcast_episode_list`. They follow the document: replaced at a save, cleared
when the card is cleared or scrubbed. [`GET /v1/resources/{id}/episodes`](api.md#episodes) serves them.
`reextract all` reads every show again at once (feed cards are `media`, Podcast Index pages `official`,
player pages `web`).

### Access walls

A challenge page (a bot wall, captcha or sign-in redirect) is a failed fetch. The resource turns
`unreachable`, is retried with the normal backoff and dead-letters after `limits.max_failures`. Its last
accepted document, event dates and occurrences stay. A wall never counts toward a disappearance, and its
title never becomes a card's name. `pinakes_fetch_total` counts it as outcome `blocked`.

A sign-in redirect whose query says the site refuses the whole address for a while, not the page
(Instagram's `is_from_rle`), is a wall at once: the sign-in page is not asked for, and no request goes
to that host for a day. Every link of the host waits meanwhile (`rate_limited`, charged nothing). The
pause is the daemon's own and ends with a restart.

A wall hook may answer in its place, also for plain 401 and 403 answers, within six seconds. The site's
own feed lists the exact link (`feed`), or a source's record names it (an IMDb title by its Wikidata item,
`media_records`).

The markers are in `data/interstitial-markers.json`. A *challenge status* is 403, 429 or 503; a *short
page* has under 2,000 visible bytes, excluding `script`, `style`, `noscript` and `template`. A page that
*describes an object* has an `og:title`, `twitter:title`, JSON-LD or microformat name.

| Sign | Counts as a wall |
| --- | --- |
| A redirect from the host to a wall host of the same site (`unblock.`, `challenge.`, `consent.`, `login.`) | Always. |
| A redirect to a sign-in path (`/login`, `/authwall`, `/signin`, `/checkpoint`, …) | Unless the requested path is itself one. |
| A title that is only the service's name (`YouTube`, `- YouTube` on `youtube.com`, `youtu.be` and their subdomains), below the site's root | At any length. Where the fetch ended decides. |
| A wall phrase in the title or first heading (`just a moment`, `access denied`, …) | On a short page or a challenge status. |
| A wall phrase in the visible text | On a short page that has a challenge status or describes no object. |
| A captcha widget | On a short page, a challenge status or a marked title, when the page has a challenge status or describes no object. |
| A challenge vendor's wording that a work could also be named (`verifying your browser`, `pardon our interruption`, …) | In the title or first heading on a challenge status. Otherwise on a short page that shows a challenge running (a vendor's script, or a `noscript` asking for JavaScript on a page without links) and describes no object. |
| An HTML answer with status 202 that is empty or has no visible text | Always. An empty 202 of another type is content. |

A meta refresh without a target is content. A refresh slower than ten seconds is a hop only on a short
page that doesn't refresh itself.

## Calendars and events

Why events behave this way is in [design](design.md#events-and-calendars).

| Switch | Enables | Off |
| --- | --- | --- |
| `event.enabled` | Event pages: JSON-LD Event types, microformats h-event, ActivityPub Event negotiation | Feed and platform links get web cards too |
| `event.calendars` | Calendar feeds | Feed URLs get web cards |
| `event.geocoding` | Venue geocoding | No venue is geocoded |

ActivityPub is negotiated once per resolution: for an advertised alternate, an event-looking page without
an extracted Event, or an Event without a zone. An event page can follow a same-host link to its own ICS,
through the [discovery cache](#discovery-cache).

### Calendar feeds

Calendar GETs follow the web policy and are capped at `limits.event_feed_body_bytes`. The body is parsed
on a blocking worker under `limits.event_parse_timeout_ms`. `webcal://` is fetched over HTTPS and never
downgrades.

- A malformed VEVENT is skipped with a warning. Exceeding a work or feed cap rejects the snapshot.
- A recurrence rule is expanded only when one of its periods holds at most
  `limits.event_max_period_dates` dates. An event with a larger rule keeps DTSTART and its RDATEs, less its
  EXDATEs, with the warning `series_rule_unexpanded`.
- A JSON feed (Gancio, JSON Feed) is parsed as one document. Its parsed form can take several times
  `limits.event_feed_body_bytes` of memory, so size that limit with the worker count in mind.
- After an upgrade that changes how rules are read, run `pinakesd reextract event`: feeds re-parse at once
  instead of at their next fetch.

### Held snapshots

A snapshot is held, not applied, when it would lose more than half of the feed's keys, add more than
`limits.feed_max_new_keys` keys to a stored feed, or exceed `limits.feed_max_rows`. An identical snapshot
releases a hold for lost or new keys. Holds count in `pinakes_feed_held_total` (alert `FeedHeld`).

| Hold | Read again after |
| --- | --- |
| First | `limits.held_retry_secs`, at least `limits.feed_cadence_floor` |
| Second and later in a row | The normal cadence, doubling each time up to seven days |
| `limits.feed_max_holds` in a row | Dead letter |

### Time zones

| `event.utc_series_wall_clock` | A series with a UTC DTSTART expands on local time when its zone comes from |
| --- | --- |
| `"x_wr_timezone"` (default) | An explicit `X-WR-TIMEZONE` (Google's exports) or a TZID |
| `"coordinates"` | Also `event.default_timezone` and coordinate-derived zones |
| `"never"` | Nothing: UTC series expand in UTC |

`event.default_timezone` places floating times. A change of either applies at each calendar's next
re-parse; `pinakesd reextract event event_platform` applies it at once. Instances that such a re-parse
moves get new keys once: the old rows are cancelled after two snapshots, or the feed is held if more than
half its keys move.

### Geocoding

Page coordinates win over geocoding, and online events aren't geocoded. A structured Nominatim search is
accepted only for one sufficiently matching address (`limits.geocode_score`). An OSM venue link also needs
the name and full address to match. Geocoded facts stay in `osm_place`; documents store versioned
references and compose coordinates at read time.

### Same-event hint

`event.same_event_hint = "on"` puts `maybe_same` on event cards: other events that are possibly the same
one ([the rule](api.md#possibly-the-same-event)). It covers the [app hint](api.md#possibly-the-same-app)
too, which needs no pass.

A background pass computes the pairs for events that start from a day ago to a year ahead.

| Part | Amount |
| --- | --- |
| Queued resources a pass takes | 25 |
| New or changed events a pass reads | 200 |
| Pause between passes | None while there is work, else five seconds |
| Candidate window | Events starting within 50 hours |
| Candidates one read examines | 100, among at most 1000 events visited. A read that runs out with room left goes on in a later one. |
| Pairs an event keeps | Its best 20 |
| Events that may name one event | `limits.same_event_incoming_kept` |
| Sweep | Hourly: drops events that left the window, `limits.same_event_drop_batch` a transaction, and queues those that entered it, changed unseen or stopped being events |

Triggers queue a resource when an event's start, place, name or document changes. An event that changes
or goes takes its pairs from the events that named it, and those refill their lists. Each change to the
pairs moves the search revision in its own transaction, so a search page's `ETag` covers the hints it
shows. Requests only read stored pairs.

`"off"` stops the pass and removes the field from cards, details and `/v1/openapi.json`. The stored pairs
stay, and changes made meanwhile are read when it is turned on again.

### Event platforms

`data/event-adapters.json` declares each platform's mode, identity, cancellation signal, cadence floor,
expected fields, key requirement and canary. Configure one under its ID:

```toml
[event.adapters.platforms.resident_advisor]
enabled = true
```

A missing key disables only that platform. A platform that is off sends its HTTP links to the web resolver.
Every platform is read with Pinakes's own User-Agent, and a platform that refuses it gets the same result
as any other page.

| Platform | Default |
| --- | --- |
| `ticketmaster` | Off without a key |
| `resident_advisor` | Off: an unofficial GraphQL API, marked fragile |
| `billetto`, `reservix`, `rausgegangen`, `entradium` | Off and fragile: each walls its event pages off from Pinakes's User-Agent |
| `starticket` | Off: its site redirects to See Tickets |
| `posh` | On, marked fragile |
| Every other platform | On |

A platform's `canary_note` in the table, where it has one, says how its canary sample was chosen, or
that the operator must configure one. Admin `/v1/info` shows the effective manifests.

**Reading.** Luma, Meetup, Eventbrite, Partiful, Posh, RingCentral, Airmeet, Mobilizon, Gancio, Wikidata,
Dice, Xceed, Ents24 and Ticketino supplement JSON-LD with bounded state parsing. JavaScript is never
executed. Authenticated APIs never redirect, and credentials are removed from retained URLs and errors.

**Self-hosted software.** Pretix, Eventyay, Gathio, Gancio, Mobilizon and hitobito are detected from what
the software itself puts in its pages, never from its name in a page's text. A detected page may cost one
bounded request more, to the host the page answered from. When that request fails, the page's own card
stands, with no warning and no drift counted.

**Meetup.**

- A date's page also reads its own calendar file, `{page}/ical/{id}.ics`. Its UID counts only when the
  file holds one event and is answered from the page's own URL origin. The read is optional: a failed
  answer keeps the stored UID and never fails the page.
- A group calendar is read only when Meetup answers it itself: over https, from a meetup.com host, at the
  calendar's path. A redirect to such an address is followed, up to three. A redirect anywhere else, an HTTP one or one in the page, isn't. The stored
  dates stay, `last_error` is `off_origin`, and the link is read again with the queue's backoff. That
  streak never counts toward the dead letter.
- A numeric page that becomes its token's alias leaves its group, and an operator's curation of it goes
  with it. Curate the token page again after its first read.
- `pinakesd reextract event_platform` reads every event platform link again at once.

**Cursor feeds.** An event portal and BTC Map keep their checkpoints in `event_fetch` and commit them with
each accepted batch. An expired cursor resyncs under the ordinary hold rules. BTC Map requests every delta
status and deletion, never advances its cursor past the local fetch time, and allows five minutes of clock
skew. Malformed rows are skipped (`invalid_cursor_row`, drift) and never imply deletions. Portal
coordinates are page facts, never written to `osm_place`.

**Derived links.** With `event.adapters.derive_resources`, these are queued as resources, charged to the
referring authors (or a per-adapter sponsor):

| Link | Lives |
| --- | --- |
| NIP-52 calendar member (`limits.max_calendar_members`) | 24 hours from the calendar's last fetch; ends when the calendar stops listing it |
| BTC Map event website (`limits.max_linked_resources`) | Until its event, capped at the horizon and extended as the horizon reaches it |
| Organiser calendar an event page names by its platform's rule, such as Meetup's `/{group}/events/ical/` (`limits.max_linked_resources`) | Until the page's event ends, at least 24 hours from the page's last fetch, capped at the horizon |

A website that two events share keeps the farther instant. `limits.max_derived_resources` caps the links of
one adapter. A lapsed link keeps its child through the orphan grace, counted from its expiry, and a revived
link meets the same caps as a new one.

**Nostr.** Identifiers and client URLs use signed NIP-52 events through two HTTP indexers (nostr.wine at
one request per second, Nostr Archives at the default rate, at most two), paging within
`limits.max_indexer_pages`. The newest matching revision wins; `nevent` and `note` pin one. Both indexers
must miss before absence is accepted. For an event pinned by ID, an Archives 404 is a miss; any other
indexer failure, a full tied page or an exhausted budget is inconclusive and retries after five minutes
without charging an attempt. With `event.adapters.websocket`, one relay hint is asked whenever no indexer
has a match (`limits.websocket_deadline_ms`, 1 MiB, 256 frames, DNS checked and pinned). Its signed match
is used; its silence never confirms absence.

**When a read fails.**

- Only parser failures fall back to a web card. Upstream failures keep the accepted document and retry.
- A refresh never replaces a dated Event with a start-less card that decides nothing (a consent wall
  answering 200, a date that failed to parse). The row keeps its card, records `last_error=drift` and is
  read again an hour later.
- The exception is a reading that decides the page is no event and says so on the card
  (`event.not_event`): `lists_events`, a page listing several events, or `creative_work`, a Wikidata item
  that is a work. It replaces the stored event, dates and all. A later reading that finds an event stores
  it ([Lists](api.md#lists)).
- HTTP 451 and cursor 401, 403 and 404 follow two-observation disappearance, and so do direct Wikidata 404
  and 410. A missing entity never removes a live Wikipedia article.

**Wikidata images.** A Wikidata item's card shows its image only with the file's licence verified on
Commons and its credit ([credits](api.md#credits)).

## Also on

The also-on worker runs every enabled source that applies to a resource ([API](api.md#also-on)) after a
save that changed its document or status, and again when the shortest lifetime among them has passed. What
each source asks and costs is on [its page](sources/README.md#also-on).

Run `pinakesd reextract also-on`, with the daemon stopped, after you set a source's key or upgrade to a
build that adds or changes also-on sources. Until then, stored links keep the results of their last run for
up to the source's lifetime (30 days unless its page says otherwise). After an upgrade that changes how
links are spelled, typed or named, run `reextract all`.

### Runs

- The queue is `also_on_dirty`. The worker takes up to 100 due resources at a time and runs
  `limits.also_on_concurrency` of them at once.
- A batch asks MusicBrainz about its links' URLs together (50 a request) and Wikidata for their items
  together (10 a request). Requests several sources make are shared through the
  [discovery cache](#discovery-cache).
- Sources run in the order [the catalogue](sources/README.md#also-on) lists them. Each sees the keys the
  ones before it found in the same run.
- Each run replaces its source's rows (`resource_key`, `derived_link`, `resource_fact`, `link_release`),
  within the three `limits.also_on_*_per_source` caps. A source that doesn't apply any more drops its rows.
- Rows live twice the source's lifetime. The hourly maintenance deletes expired ones; a deleted key
  regroups its resource.
- A social post found gone and a link that carries a credential get no lookups.

| Outcome | What happens |
| --- | --- |
| A source fails or panics | Its earlier rows stay, the others' results stand, and the run is retried after `limits.also_on_retry_secs`, doubling to an hour, or after the wait a service named when longer. |
| A secondary service of a source fails | What the link and the other services gave is recorded and nothing the source recorded before is deleted. The run comes back with the same backoff. `pinakes_also_on_service_errors_total{service,class}` counts it. |
| Six such runs in a row | The run comes back once a day, or at the sources' lifetime when shorter. |
| A service refuses the operator's credentials | Nothing is deleted, and no run comes back early. The refusal is held for `limits.also_on_refusal_hold_secs`. |
| Every service answers | The rows are replaced and the streak starts over. |

### Switch a source off

| Setting | Effect |
| --- | --- |
| `[also_on] enabled = false` | Stops the worker. Stored keys keep grouping and stored links stay on cards until they expire. The API hosts below are refused. |
| `[also_on] derive_links = false` | Keys and facts only: no derived links on cards, and a tagged link an exact derivation names doesn't join by it. |
| `[also_on.sources.<id>] enabled = false` | One source off. Its rows stop showing at the next start. |

A source that is off isn't asked, and an API with a host of its own is refused at the egress whatever path
asks: `api.deezer.com` (Deezer) and `api.themoviedb.org` (TMDB). The site's pages and the links that other
sources give to them stay.

A source that counts as switched off stops showing at the next start: no card, `also_on` list, search, group
or other source reads its keys, facts or links. Its rows stay until they expire, and one switched on again
shows them at once. TMDB also counts as switched off while it has no credential; any other source without
its credentials doesn't run, and its stored rows show until they expire.

When a source is switched, the resources that its keys and keyed links name are regrouped. Cards that hold
its keys or facts are due at once, unless they are final or dead-lettered, so what it wrote into a card (a
title's name and description, a type) changes as the queue reaches them. A card records the findings it
carries (`hinted_by`); one whose finding is off, expired or changed is read in full, so a 304 never keeps
it.

### Costs

| Source | Costs |
| --- | --- |
| `spotify_isrc` | One Spotify search per ISRC key of a track on another service; per barcode key of an album, a search and up to three album reads. Cached an hour. |
| `tidal_isrc` | One TIDAL request per ISRC or barcode key of a track or album on another service. |
| `itunes_song` | Per track, a Deezer, Spotify or TIDAL read and an iTunes lookup. Its links are maybes, never keys. |
| `youtube_match` | Per label upload, a watch page (up to `limits.also_on_watch_page_bytes`) and a Deezer and a TIDAL search. Its links are maybes, never keys. |
| Duplicate check (music sources) | For a recording MusicBrainz relates a link to, one ISRC lookup per ISRC and, where several recordings share one, each of their records: at most 8 recordings, 16 ISRC lookups and 32 recording reads per walk, all cached. |

Each runs the part of a service only while that service's source runs. All are on by default; the Spotify and
TIDAL ones run only with their credentials.

### Paces of the services

Sources reach their services through the egress, one request at a time per host. These paces lower a
host's default; a pace above `limits.host_requests_per_second` (1) leaves the default, and
`limits.host_rates` overrides both ([pacing](#pacing)).

| Host | Pace |
| --- | --- |
| `musicbrainz.org`, `coverartarchive.org` | 1 request/s each |
| `www.wikidata.org`, `*.wikipedia.org`, `commons.wikimedia.org` | 200/min each |
| `openlibrary.org` | 3/s |
| `itunes.apple.com` | 20/min |
| `api.tvmaze.com` | 2/s |
| `web.archive.org` | One request every 2 s, tagged captures' cards included |
| `api.nostr.wine`, `portal.einundzwanzig.space` | 1/s each |
| `api.nostrarchives.com` | 2/s |
| A service only one source reads | The pace on its page |

A MusicBrainz mirror (`also_on.sources.musicbrainz.base_url`) serves every MusicBrainz read: the sources
and the cards of MusicBrainz pages. It is reached through the egress policy, so a private address is
refused. Its host takes the default pace; set its own in `limits.host_rates`.

### Music enrichment

`[web] music_enrichment` gives a MusicBrainz artist, release group, release or recording card an image from
Wikimedia Commons and a description from Wikipedia or Wikidata, from the entity's Wikidata item, each with
its credit ([credits](api.md#credits)). A release or release group with no free Commons file takes its front
cover from the Cover Art Archive, and a recording that of an official release it is on.

It runs inside the card's own read, after MusicBrainz answered, within
`limits.music_enrichment_timeout_ms`.

| Host | Asked for |
| --- | --- |
| MusicBrainz | For a recording: its works, only when it has no item of its own with an English article, and one page of its official releases for a cover |
| Wikidata | The item the entity's own relation names, read back. Without one: a search and a read-back by its MBID, then for an artist by up to two Discogs pages. Then the item's pages and its image claim. For a recording, also its work's item. |
| Wikipedia | The English summary, else one other language's; for a recording its work's English one too |
| Commons | One file |
| Cover Art Archive | One cover for a release (only when MusicBrainz says it shows one) or release group; up to three for a recording |

The Wikimedia requests run one after another, and the cover request runs beside them, so a busy Wikidata
doesn't leave an album without its cover. A cover's redirect to archive.org is kept, never followed: with
`egress.allow_hosts` set, allow `coverartarchive.org`; archive.org isn't needed.

A failed or slow request never fails the card. A field that a timeout, a 429, a 5xx, Wikidata's `maxlag` or
another unexpected status left unread keeps its stored value and credit, and the card is read again after
`limits.music_enrichment_retry_secs` or later. A definite answer (no item, no text, a refused file) removes
the field.

Stored cards change at their next read. To apply a change of the switch at once, run `pinakesd reextract
web`. Raising `limits.music_enrichment_timeout_ms` (up to 13000) for such a run leaves fewer cards unread.
Switched off, the cards are MusicBrainz's alone from their next read.

### TMDB posters

The `tmdb` source gives a `Movie` or `TVSeries` card with no image or no description of
its own TMDB's poster and overview, by the title's IMDb ID or Wikidata item. It runs only with a credential
in `TMDB_API_KEY` ([API keys](api-keys.md#tmdb)).

- **After setting the key:** with the daemon stopped, run `pinakesd --config <path> reextract also-on`, then
  start it. Or only restart it and wait for each card's next also-on run, at most 30 days.
- **Without a credential, or with `[tmdb] enabled = false`:** no request goes to `api.themoviedb.org`, stored
  rows are hidden at the next start, and every card that holds them drops TMDB's values at its next read.
  `/v1/info` lists TMDB's attribution only while a stored card still holds a TMDB value.
- **Lifetimes:** rows are read again every 30 days and live 60 days from the last read that answered. A card
  takes TMDB's values only from live rows, so an outage keeps them up to 60 days.
- **Cards never read again** (final, dead-lettered or orphaned): the hourly maintenance takes out a value
  their live rows don't give, with a change line. It only removes, and it also takes out the values of a
  source the build doesn't have.
- **Change lines:** while a credential is set, startup refuses a `limits.change_retention_days` over 120. A
  change line that holds a TMDB value goes after 120 days whatever that setting says, with TMDB on or off.
- **Pace:** the default rate, and at most 4 requests/s when the default is raised. Answers are kept an hour.

## Groups

What joins is in [design](design.md#keys-and-groups) and the [API](api.md#groups); `maybe` links and hints
are in [design](design.md#maybe-same).

### Grouping runs

Grouping runs after a document commits, from its own queue (`group_dirty`), so a grouping failure never
blocks a save. A group stores membership only; cards read it when they are served.

Groups form only from this evidence:

| Evidence | Joins |
| --- | --- |
| `registry` with `cites_doi` or `cites_isbn` | An identifier card and a landing page its registry names that cites it |
| `ics_uid` within one feed | Occurrences with one UID |
| `ics_uid` of an event page's own calendar file | That page and its date in the organiser calendar its platform names for it, within 300 s; when both are located, also within `limits.group_max_distance_m` |
| `hub:{key}` | Resources with the same exact key of an also-on source |
| Curation | What the operator put together |

A join that would put two values of a single-object key (two recordings, two Q-ids) in one group is refused,
also for the namespaces of removed sources (`single_object_namespace`).
`pinakes_group_key_conflict_resources` counts the resources whose last run refused one, which points at a
wrong ID on a hub. Two feeds or two publishers never join on a UID. A page's calendar file gives its UID only
when answered from the page's own URL origin (scheme, host and port, after redirects).

| Outcome | Retry |
| --- | --- |
| An input changed during the run | 5–10 seconds, growing to an hour |
| Another writer holds the lock | 1–3 seconds |
| The run failed | A minute, doubling to an hour |
| The run stopped on its budget (`work_budget`) | Within the hour, from the key it stopped at |

### Work budgets

Each key of a run may make a capped number of compatibility checks: twice `limits.group_max_members`
squared, held between 100,000 and 2,000,000. `pinakes_group_budget_resources{state}` counts what passed it.

| `state` | Meaning |
| --- | --- |
| `parked` | A key alone exceeded the cap. Its unions are undone and the later keys still run. |
| `stopped` | The run reached four caps and goes on later. Parked keys' checks count. |
| `settled` | After 24 stopped runs on unchanged input, the groups the last run left stay until the input changes. |

Past either limit, units holding a key the run didn't finish keep their groups. A member deleted by orphan
expiry leaves its group at once.

### Curation

Curate a group on the admin listener with `PUT /v1/groups/{id}` and undo it with `DELETE /v1/groups/{id}`;
its members then group again by their keys. Curation waits up to two seconds for the group lock and answers
503 with `Retry-After`.

### Same-recording hint

`groups.same_recording_hint = "on"` puts `maybe_same` on `MusicRecording` cards: other recordings that are
possibly the same one ([the rule](api.md#possibly-the-same-recording)), which no key joins.

A background pass computes the pairs, 25 queued resources at a time, with the next pass at once while there
was work, else after five seconds. Triggers queue a recording when its name, type, status, page artist or
duration changes, or an artist or duration fact of it is read or expires. A recording keeps its best twenty
pairs and is named by at most forty; a read examines at most a hundred candidates and goes on in a later one.
An hourly sweep, which also runs at startup, queues recordings never read and drops those that
aren't recordings. Each change to the pairs moves the search revision in its own transaction.

Requests read the stored pairs, leaving out the card's own group members. `"off"` stops the pass and removes
the field from recording cards (and from `/v1/openapi.json` when the event hint is off too); the stored pairs
stay.

### Tenant rules

`data/multi-tenant-hosts.json` says which part of a multi-tenant host is one publisher
(`group_keys::origin`). Two links have the same publisher when they have the same tenant; on any other
host, the same registrable domain. The publisher must match when:

- a page's ActivityPub object is read for its card;
- a feed a page advertises lends the page its episode GUIDs;
- a page joins a podcast episode;
- an event page's calendar gives the page its `ics_uid` key.

| Rule | Tenant |
| --- | --- |
| `"subdomain"` | The full host name |
| `"first_path_segment"` | The host and the first path segment |
| `{"path_segment": N}` | The host and the first N path segments |
| `{"path": {"segment": N, "skip": ["u/*"], "extend": ["s"], "query": ["src"]}}` | The host and the first `segment` path segments, after `skip` drops a noise window (Google Calendar's `u/{n}`). `extend` takes the next segment too (anchor.fm `/s/{id}`), and `query` scopes by a parameter when the path is short (`embed?src=`). |

A link with too few path segments (and no such query parameter) is a publisher of its own. A subdomain of a
listed host takes the host's rule; the longest matching host wins.

## Monitoring

Scrape `GET /metrics` on the admin listener. [ops/README.md](../ops/README.md) has every series, the scrape
setup, the alert rules (`ops/alerts.yml`) and the Grafana dashboard (`ops/grafana/pinakes.json`).
`scripts/check-ops` checks every metric they name against the registry, and runs `promtool check rules
--lint-fatal` when it is installed.

### Alerts

| Alert | Severity | Look at |
| --- | --- | --- |
| `IngestionStalled` | critical | The Nexus and its credentials; [ingestion](#ingestion). |
| `IngestPanics` | critical | A poll panicked outside a feed line; the cursor holds. |
| `MetricsMissing` | critical | The daemon is down, or the scrape fails. |
| `MetricsStale` | warning | `/metrics` answers but can't read the database. |
| `ApiErrors` | critical | More than 5% of public requests answered 500 or 504. |
| `ApiPanics`, `ApiRateLimited` | warning | A handler panicked; `api.requests_per_minute` or `api.trusted_proxies`. |
| `QueueBacklog`, `DeadLetters` | warning | [Queue](#queue-and-refresh), rates and [dead letters](#dead-letters). |
| `CanaryDown`, `SchemaDrift` | warning | A source changed its answers; [canaries](#canaries). |
| `WallUp`, `RateLimited` | warning | [Access walls](#access-walls); `limits.host_rates`. |
| `FeedHeld`, `CancellationBurst` | warning | [Held snapshots](#held-snapshots). |
| `CursorReset` | info | A feed resync or [feed reset](#feed-resets). |
| `PreviewCapacityReached` | warning | [New previews](#new-previews). |
| `ProviderKeyRefused` | warning | The provider's key ([API keys](api-keys.md)). |

A new key needs a restart.

### Info and stored errors

Admin `GET /v1/info` has the manifests, resolver status (aliases left out), counts (`aliases` among them),
ingestion, source and canary state. Public `/v1/info` shows only the version, what the instance resolves, the
limits a client must respect and the OSM licence. Admin info and the database metrics are cached for 15
seconds.

Stored errors are classes, never texts: the [queue's](#failures-and-retries) `last_error`, and a source's
health (`source_state`), one of `dns`, `connect`, `timeout`, `http_4xx`, `http_5xx`, `parse`, `policy`,
`too_large` and `too_complex`.

### Logs

A failed resolve is logged at warn as `resolution failed`, with the resource ID (`resource_id`), the link's
host (`host`; its scheme for a link without one), the class (`kind`) and the upstream status (`status`). A
link that waits for the daemon's own pace is logged at debug (`resolution waits for its host`). Source health
that can't be written is logged as `source health not recorded`; the jobs still finish.

- Log lines name a link only by its host. Lines about a fetch, a lookup or a parse print no error text.
- Lines about the daemon's own services (the database, Nexus, the dump) print theirs through a guard that
  cuts every URL before its query or fragment and drops its user info.
- Feed lines name the Nexus by its host, and a Nominatim request by its path below the configured base
  (`lookup`, `search`). Only admin `GET /v1/info` shows the Nexus URL, in its ingestion cursors.
- The MCP library logs only its warnings, whatever `RUST_LOG` asks for.

### Canaries

Canaries (`canary.enabled`) run every `limits.canary_interval_secs`, first one interval after a source is
first seen, on the ordinary workers and host budgets. They may take 15 seconds, their state survives restarts,
and they never write documents, the OSM cache or changes.

| Result | Meaning |
| --- | --- |
| Failure | The read failed or a declared field is missing. The web canary must return a `name`. |
| `outcome=drift` | A successful read lacks a declared field. |
| Inconclusive | The sample is over its size cap: neither a failure nor a success. |
| Skipped | The egress policy denies the canary's host. Its gauges are cleared. |

Any other refusal of a canary's request (a content type, a redirect) is a failure. A platform with a
`canary_link` (Infomaniak) reads its listing's first event link of the declared form, so it is as current as
the provider keeps its listing. Any other reads its fixed sample, which its `canary_note`, where it has one, describes.

### Links that carry a credential

`scripts/verify-no-capability-urls` (run by the tests) scans every tracked and new source file for capability
URLs. Synthetic fixture URLs are allowed only in named test files, and failures print paths without the URLs.

At runtime the daemon applies the same provider patterns, plus credential query keys and a password in the
user info (`crates/pinakes-core/src/capability.rs`): `POST /v1/resolve` refuses such a link, and no also-on,
archive or oEmbed lookup is sent one. `crates/pinakes-core/tests/fixtures/capability_urls.json` holds the
probes both must agree on; add a pattern to both.
