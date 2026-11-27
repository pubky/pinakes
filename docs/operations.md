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
| `pinakesd [--config PATH] reindex` | Builds the text index from the database into a new generation and switches to it. Run it with the daemon stopped ([text index](#text-index)). |
| `pinakesd [--config PATH] catalogue status` | Prints each catalogue's switch, phase, cards, upstream version, last import and update, how current its cards are, wait and last error ([catalogues](#status-and-drop)). |
| `pinakesd [--config PATH] catalogue drop NAME` | Deletes a catalogue that is off: its cards, its state and its directory. Runs beside the daemon ([catalogues](#status-and-drop)). |
| `pinakesd [--config PATH] catalogue import NAME [--from FILE\|DIR\|URL]… [--limit N] [--reads N] [--update [--since TIME]] [--force]` | Runs one full import (or update) of a catalogue now, with the daemon's code, under the catalogue's lock ([catalogues](#status-and-drop), [run an import](#run-an-import)). |
| `pinakesd [--config PATH] catalogue import NAME --request` | Asks the daemon running the catalogue's importer for a full import, beside it ([catalogues](#status-and-drop)). |
| `pinakesd [--config PATH] catalogue allow-shrink NAME` | After a run that failed as `shrunk` (refused otherwise), lets the catalogue's next full import remove the cards it did not read, however many: it goes through at its next try, unless a run fails for another reason first. Runs beside the daemon ([catalogues](#a-full-import-that-shrinks-the-catalogue)). |
| `pinakesd [--config PATH] catalogue clear-refusal NAME` | Clears what the catalogue's importer refused (a dump, a file, a change file, an update's window), so its next run reads it again; refused once more if it still can't be applied. Runs beside the daemon ([catalogues](#a-refused-dump-or-file)). |
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
| `search.index_dir` | `"search-index"` | The text index's directory, relative to the working directory ([text index](#text-index)). The container image sets `/var/lib/pinakes/search-index`. |

### Catalogue sections

A catalogue is on when both `catalogues.enabled` and its own `enabled` are true
([catalogues](#catalogues)).

| Key | Default | Meaning |
| --- | --- | --- |
| `catalogues.enabled` | `true` | All catalogues. Off, Pinakes indexes only referenced links. |
| `catalogues.dir` | `"catalogues"` | Downloads and temporary files of imports, one directory per catalogue (`catalogues/wikidata`), relative to the working directory. The container image sets `/var/lib/pinakes/catalogues`. |
| `catalogues.<name>.enabled` | `true` | One catalogue: `wikidata`, `musicbrainz`, `osm`, `openalex`, `podcastindex`, `tranco` or `software`. |
| `catalogues.wikidata.limit`, `catalogues.musicbrainz.limit` | none | Import at most this many items, for a small instance. None imports all. Wikidata reads its dump as a stream up to the limit instead of downloading it, and its updates add new items only below the limit ([a part of Wikidata](sources/wikidata_catalogue.md#a-part-of-wikidata)). |
| `catalogues.osm.region` | `"planet"` | `planet`, or a region of Geofabrik's OpenStreetMap extracts by its path (`europe/switzerland`). A change makes the next update a full import of the new region. |
| `catalogues.openalex.min_citations` | `50` | Import works cited more than this many times, 0–1000000. |

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
| `refresh_max_secs` | `7776000` | 3600–31536000 (a first `refresh_resource_secs` tier above it is the longest wait instead) | Longest wait between reads of an undated card that keeps answering the same, and the wait of a link found gone ([backoff](#refresh-backoff)) (seconds). |
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
| `search_text_candidates` | `500` | 50–10000 | The best units a search with `q` reads from the text index ([Text index](#text-index)). |
| `map_cluster_cells` | `4096` | 16–65536 | The most map cells one `GET /v1/map/clusters` view may span at its zoom ([API](api.md#map-clusters)). |
| `map_cluster_timeout_ms` | `3000` | 100–60000 | How long one `GET /v1/map/clusters` count may take; past it the answer is 503 ([API](api.md#map-clusters)). |
| `search_index_commit_secs` | `2` | 1–60 | Seconds between two passes that commit queued changes to the text index. |
| `search_index_searches` | `0` | 0–256 | Text searches the text index runs at once; `0` is one per CPU core. One more waits for a turn; one client address runs at most half of them while others wait ([Text index](#text-index)). Behind a proxy, set `api.trusted_proxies`, or every client counts as the proxy's one address. |
| `search_index_budget_ms` | `2000` | 100–30000 | The most a text search's index stage takes, its wait for a turn included; past it the search answers 503. |
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

`migrations/0001_schema.sql` is the whole schema. `pinakesd --migrate`, the daemon's start,
`reextract` and the `catalogue` commands each create it on an empty database and leave a database that already has it as it is.
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

### Busy services

A service that answers 408, 429 or 5xx is busy for now. The readers of a service (official sources, event
platforms, also-on sources, lookups, the hubs) and the egress's own 429 turn such an answer into a wait of
the same length:

| Answer | Wait |
| --- | --- |
| Names a wait (`Retry-After`, or a spent quota's reset) | That wait, at least a minute and at most an hour |
| A 429 that names no wait | Five minutes |
| A 408 or 5xx that names no wait | A minute |
| The egress's own 429 read again from the discovery cache | What is left of its time there, at most 300 seconds |

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
- **Map links.** `pinakesd reextract web geo` reads stored map links again: Google Maps and Apple
  Maps links on the web route get their source's card (`google_maps`, `apple_maps`), and points their
  name from the link (Organic Maps, `geo:…?q=…(name)`).

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
`limits.osm_dump_every_hours`, then at that interval. With the OpenStreetMap catalogue on, the file
holds its imported places too, after the cached objects, each with `from: "catalogue"`. Why the dump exists is in
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
It takes turns with the same-recording and same-event passes (one advisory lock per hint), so a batch
waits for a hint read in progress. An orphan whose hint rows another transaction holds, and the rest of a
run whose batch hit a database deadlock twice (logged), are left for the next run.

| Resource | Kept for |
| --- | --- |
| Lost its last reference | `limits.grace_days` after that |
| Never referenced (only previewed) | `limits.preview_grace_hours`, at most `limits.grace_days` |
| A derived child | `limits.grace_days` after its link's expiry, however it was created |
| A venue of a card or an occurrence | As long as something names it |

A child or venue whose holder is deleted, and a venue its card or occurrence stops naming, keeps the
full grace counted from its creation, never the preview grace.

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
| `wall` on a social card | A social site refused a read: a stored card that says more, or one found gone, stays with its status; else the link gets the `blocked` card | No; read again after 1, 2, 4, then every 7 days | Never |
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
- **Refusals.** A link a social site refuses (its sign-in redirect, a challenge page, a 401 or 403, a
  page's 451, or a status of its own such as LinkedIn's 999) is read again after a day, then after 2,
  4 and 7 more days, and every 7 days while one refusal follows another. The streak is kept in
  `timeout_streak` while `last_error` is `wall`, both for a kept card and for a `blocked` card; a read
  that answers ends it, and another failure in between starts it again. A card found gone stays gone: a refusal says nothing
  about the object. Asking a site that refuses the request again soon only makes it refuse longer.
- **Own waits** keep the streak of a refused connection, an upstream wait or a refusal.

### Missing results

A missing result needs a second fresh fetch to confirm it.

| Link | Confirming fetch after |
| --- | --- |
| With an accepted card | `limits.missing_ttl_secs`, at least an hour |
| Without one yet | `limits.backoff_secs` |

Once confirmed, a link on the [refresh backoff](#refresh-backoff) found gone for good (a page that
answered 404 or 410) is read again only every `limits.refresh_max_secs` (90 days) until it answers
again, or until nothing references it and [cleanup](#cleanup) removes it. Other confirmed missing
links (a 451, a card source's "not found", a Pubky object) are read again after
`limits.missing_ttl_secs`.

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
| Web, media and other undated cards | The first tier, then the [backoff](#refresh-backoff) |
| Official cards | `limits.resolver_ttl_secs`, then the backoff; sooner where the source asks |
| Cards built from a catalogue card | Never fetched: they change with their card ([catalogues](#shared-links-a-catalogue-holds)) |
| Social content | Within the hour; Instagram's within the day. While the site refuses the read, the [refusals' backoff](#failures-and-retries) |
| A dated card | One fetch 24 hours after its end (or start) makes it final |
| A job posting | Its posting date doesn't end its refreshes |
| OSM places | `limits.osm_ttl_secs`. A confirmed missing object stays cached as missing for as long. |
| `geo:` and map points | Final |
| A card that changes by itself at a known time (an auction ends) | No later than then |

A new reference reopens a final resource; `geo:` stays final. HTTP validators are kept only for
calendars, where they commit with the parsed snapshot.

### Refresh backoff

An undated card read from its link (the web, media, ISBN, DOI, Nostr, ActivityPub and official
routes) waits longer each time it answers the same:

| Read | Next wait |
| --- | --- |
| Changed the card (a new card included) | The base: the first tier (`limits.refresh_resource_secs`, 7 days), an official card's `limits.resolver_ttl_secs` |
| Found the card the same, a 304 included | Twice the last wait, up to `limits.refresh_max_secs` (90 days) |
| Confirmed the link gone for good (404, 410) | `limits.refresh_max_secs` |

At the defaults an unchanged page is read after 7, 14, 28, 56 and then every 90 days. The last
wait is the card's stored `ttl_secs`. Events (dated or not), calendars and feeds, podcast shows (a
new episode is no change to the show's card), cards with a date, social content (within the hour),
Nostr and ActivityPub profiles (and those links found gone) and Pubky objects (their authors edit and delete them), a card that names when it changes,
OpenStreetMap objects and `geo:` points keep the schedule above. A first tier above
`limits.refresh_max_secs` is the longest wait instead. A stored
card keeps its next refresh time when the daemon is upgraded: each moves to the backoff at its own
next read.

Opening a card (`GET /v1/resources/{id}`, `GET /v1/resources/by-uri`) whose refresh is due moves
it to the front of the queue, as a lookup does: it is read again in the background and the open
answers with the stored card. A card that is not due keeps its wait.

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
(Instagram's `is_from_rle`), is a wall at once: the sign-in page is not asked for, and no request
goes to any host of that site (its registrable domain: `instagram.com` and `www.instagram.com`
alike) for a day, then for 2, 4 and 7 days after each such refusal in a row. A request already
waiting for its host's turn when the refusal comes sends nothing, and a refusal to a request sent
before the pause began adds no step. A final answer, or a redirect away from the site, from a host
that refused ends the streak once the pause is over; a redirect within the site, an answer that
comes in while the pause runs and one from another host of the site (`about.instagram.com`) do not.
Every link of the site waits meanwhile (`rate_limited`, charged nothing, asked again each hour
without a request), and those due count in `pinakes_refresh_overdue` until the pause ends. The pause
and its streak are kept in memory and a restart forgets them; each link's own wait is stored.

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
its keys or facts are due at once, final ones included, unless they are dead-lettered, so what it wrote into a
card (a title's name and description, a type) changes as the queue reaches them; a final card is read once
and is final again after. After a start, once the listeners serve, a final card that still holds what a
switched-off source wrote is read once the same way, and a dead-lettered card, which no read reaches until a
new reference, loses the name, description and stream that source wrote into it; a type it gave stays until
the card is read again. Every hourly maintenance pass does the same. The step reads the cards only after a
switch, or after such a card became final or dead-lettered; a failure is logged and the next pass or start
tries again. A card records the findings it
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

## Text index

A search with `q` (without `author`, `reach`, `related`, `in_collection`, `key`, `remote` or
`ingredient`) reads its candidates from an embedded Tantivy index in `search.index_dir`
([API](api.md#text-search)). The index holds every searchable unit and every card of the
catalogues that are on: names, other names, descriptions, people's tags, source labels, type,
host, category, venue, source, catalogue, fame, activity, start and end, and the place. A
referenced card built from a catalogue card also holds its labels, its card's other names and
its card's fame (H3 map
cells at resolutions 4, 6 and 8, about 1,770 km², 36 km² and 0.74 km² per cell; a `near` circle is
covered by the finest that needs at most 1,100 cells). Search loads the candidates from Postgres
and applies every filter again, so the index decides which units are looked at and their text
relevance, never what is shown.

The index is derived data, rebuilt from the database when it is missing.

- **Referenced links.** Triggers on the resource, reference, score, occurrence, alias and OSM
  place tables queue each changed resource (`search_index_queue`). Every
  `limits.search_index_commit_secs` (2 s) the daemon reads the queue in order, 20,000 rows at a
  time, reads those resources' units again, replaces their documents, commits, and deletes the
  rows it read; a row of a transaction still running is read by a later pass. A card that is
  deleted, scrubbed or loses its last reference leaves every search at once (Postgres no longer
  lists it, or finds it without text) and the index within a pass. A pass that removed a card's
  text (a post found gone, emptied or opted out) merges the segments that hold the card's
  documents, current or deleted, so its words leave the index files too. The merge runs in the
  background: the pass, later commits and searches don't wait for it. One merge runs at a time;
  a removal meanwhile, or a merge that failed (a merge of Tantivy's own took a segment first,
  started up to 5 times in all), is merged at a later pass. The commit that removes the text
  records the merge it owes, and a commit after the merge records that nothing is owed, so a
  daemon restarted before the merge finished merges at its first pass. Any other pass merges nothing, and
  segments with 5% deleted documents merge on their own (as a changed or removed catalogue card's
  old words do). A card built from a catalogue card is read again when its card changes: the
  join rebuilds it (its document holds the card's content hash), so its other names and fame
  follow the card's.
- **Catalogue cards.** Every card whose content changes is queued (`search_index_catalogue_queue`,
  by triggers on `catalogue_card`; a full import's stamp of an unchanged card queues nothing), and
  so is every card an update run adds or removes, and every card a later full import adds or
  removes (one that runs after the catalogue's first full import has finished), 20,000 rows per
  pass, so a run that writes faster than the index reads holds back neither referenced links nor a
  re-read. A catalogue's first full import queues only the cards it changes in place: once it has
  finished (`catalogue_state.last_import_at`), the daemon reads all the catalogue's cards again,
  20,000 per pass (in source-ID order) after the pass's queues, and when it has read the last one
  removes the documents of cards gone since, so millions of first-imported cards never pass
  through a queue and referenced links keep their 2 s meanwhile. Until then a removed card stays
  in the index, and Postgres, which checks every candidate, leaves it out. The re-read logs its
  start and end, and `pinakes_search_index_reread_cards{catalogue}` counts the cards it has read; a
  daemon restarted during a re-read starts it again from the catalogue's first card. A catalogue
  switched off leaves every search at once (searches read only the catalogues that are on) and the
  index at the daemon's next pass, which removes its documents (also those of a re-read under way)
  and, in the background, merges their words out of the files (none when it had no documents);
  `pinakesd catalogue drop` then deletes its cards. A catalogue turned back on is read again at the
  next pass.
- **Start.** The daemon opens the index. When the directory or its generation is missing,
  unreadable, of another format, or built from another database (each database has its own
  identity: a token in `search_index_database` and the database's Postgres ID, so a restored copy
  or a second database on the cluster is another one), it logs a warning and builds the
  index from the database in the background (about a second per 2,000 units on the development
  data; catalogue cards at Tantivy's bulk rate); until then text searches read Postgres alone. A
  build reads the whole database in one read-only snapshot (`REPEATABLE READ`), held for as long as
  the build runs (minutes at catalogue size), and vacuum keeps every row version that snapshot can
  see until it ends: start a rebuild when the write load is low. A
  build that fails is retried every minute; meanwhile the rows of finished transactions in both
  queues are deleted (the next build reads everything), and a failed build leaves no partial
  generation.
- **Rebuild.** `pinakesd --config <path> reindex`, with the daemon stopped (it takes the instance
  lock), builds the index from the database into a new generation, prints its progress, switches
  to it and removes the old one. Use it after restoring a database from a backup, or to compact the
  index. Queue rows written meanwhile are applied by the daemon's next pass.

The directory holds one generation (`gen-<milliseconds>`) and a `CURRENT` file naming it; a
rebuild needs room for two generations, and a generation `CURRENT` doesn't name is removed before
a build. `pinakes_search_index_open` is 1 once the index is open, and
`pinakes_search_index_last_pass_timestamp_seconds` is the open's time, then the last pass that read
its queues without an error, so passes that fail from the start stall it ([alerts](#alerts)).
`pinakes_search_index_queue{queue}` is the span of sequence numbers each queue holds (`resources`,
`catalogue`), read at each metrics refresh: the rows not read yet, or more where rows were read
out of order.

A text search waits for one of `limits.search_index_searches` turns (one per CPU core by
default) and has `limits.search_index_budget_ms` (2 s) for its index stage, the wait included;
the turn goes with the search, so one whose client has gone still holds it until it stops. Past
the budget a search stops in the index and answers `503` ([API](api.md#text-search));
`pinakes_search_index_busy_total{reason}` counts them (`turn`: no free turn in time; `budget`:
past the budget; `share`: a turn was free, but its client address ran half the turns while
another client's search waited, for the whole budget), alert `TextSearchBusy`. A client address
(an IPv6 address by its /64) runs at most half the turns (at least one) while other clients wait;
alone it takes every free turn, and a waiting search holds nothing. Every word, phrase and filter
of a search checks the budget between its posting steps (a phrase at every step), so a part that
matches nothing for long stops too. A word set (a word's start) and a filter's set of values (`type`,
`tags`, `from`, `exclude_type`) read their words' lists in one go: the budget is checked before
they start, not while they read. A word's start expands to at most 1,000 words in all (those the
most names hold of the first 20,000 per segment in dictionary order, while they are in 500,000
names in all), a text to 32 items, a phrase to its first 8 words (all of its words when every one
is common or its rarest is in more than 50,000 names), and 64 three-letter pieces. A text of one
character found inside names is left to Postgres, so no search reads the index's whole
dictionary; it takes a turn as an index search does, and so does a search without text while a
catalogue is on (it reads the best known catalogue cards from Postgres). Their Postgres reads, the
page's own read among them, stop with what is left of the budget (2 s by default, where other
reads have the pool's 5 s): each runs under a `statement_timeout` of it, so Postgres cancels the
statement too; past it `503`, counted as `budget`. With every catalogue off a search without text
takes no turn.

## Catalogues

A catalogue card is one item of an open catalogue, stored in `catalogue_card`. The catalogues are
`wikidata`, `musicbrainz`, `osm`, `openalex`, `podcastindex`, `tranco` and `software` (crates.io,
F-Droid and Flathub). An importer is the only writer of a catalogue's cards; they don't go
through the fetch queue, the refresh tiers or the change journal. Each catalogue has its page:
[`musicbrainz`](sources/musicbrainz_catalogue.md), [`osm`](sources/osm_catalogue.md),
[`wikidata`](sources/wikidata_catalogue.md), [`openalex`](sources/openalex_catalogue.md),
[`podcastindex`](sources/podcastindex_catalogue.md), [`software`](sources/software_catalogue.md)
and [`tranco`](sources/tranco_catalogue.md). Catalogues are
on by default, so a daemon whose configuration has no `[catalogues]` section runs their importers:
with `http.contact` set and the disk they need, the Wikidata importer downloads its 104 GB dump;
without a contact they ask nothing, and `catalogue status` says they wait for one. Set
`[catalogues] enabled = false` for an instance that imports nothing.

### What a card holds

| Field | Holds |
| --- | --- |
| `catalogue`, `source`, `source_id` | The catalogue, its sub-source (the registry for `software`, else the catalogue) and the item's ID there, unique per catalogue |
| `uri` | The item's canonical link: an `http` or `https` link, normalized as a referenced link is and stored as its root by the spelling rules (`https://doi.org/10.1/X` as `doi:10.1/x`, a tracked link without its tracking parameters), as a resource of the same link is. A link of another scheme, one that carries a credential as written (a password in it), or a link of a source of people's own posts (a Bluesky profile, a Mastodon post) is refused |
| `type` | The schema.org type, by its bare name (`Person`) |
| `name`, `other_names`, `description` | Texts, without control characters or invisible format characters (a zero-width space, direction marks; names keep the joiners Persian and emoji are written with), each read up to four times its cap. An other name or a description that carries a credential is left out; a name that does refuses the card |
| `labels` | The source's labels as valid Pubky tag labels: lower case, at most 20 characters, no spaces, commas, colons or invisible characters. A label that can't be made one (or whose text is over 256 bytes) is left out. |
| `keys` | Join keys (no whitespace, control or invisible character) in the form a link's own key takes ([shared links](#shared-links-a-catalogue-holds)), whatever the importer's spelling: `wikidata:Q42`, `mb:<entity>:<mbid>` in lower case, `doi:` in lower case, `isbn:` as ISBN-13, `crate:` in lower case with `-` for `_` (`crate:serde-json` for `serde_json`), `arxiv:` without a version, `openalex:W…`, `homepage:` in lower case and punycode without `www.`; a key not valid in its kind is left out. Then the card's own link's `url:<resource id>`, and `url:<resource id>` for each link the catalogue itself names as the item (a Wikipedia sitelink, a MusicBrainz URL relation): such a link is built from the card ([shared links](#shared-links-a-catalogue-holds)), so an importer keys only links its catalogue vouches for |
| `related_uris` | Links in the item's group that are not the item, such as an official website: `http` or `https` links, each stored as its root |
| `point`, `fame`, `details` | A point, a 0–1 fame from the source's own signal, and the catalogue's own extras |

`catalogue_state` holds one row per catalogue: its phase (`idle`, `import`, `update`, `failed`,
`disabled`), the run under way and where it resumes, where the next update starts, its cards, the
upstream version, when the last full import and update finished, how current its cards are
(`current_as_of`: the upstream time of the data the last finished run left), why it waits, its
errors, the last finished full import's scope (`scope`), the cards the full import under way has
read (`generation_cards`) and an operator's allowance to shrink (`shrink_allowed`).

`catalogue_card` leaves a fifth of each page free (`fillfactor = 80`), so a full import's stamp of an
unchanged card can stay on its page: its heap is about a quarter larger than a packed table's
(1.03 KB per card measured over 1.3 million real cards, against about 0.8 KB packed). About one
stamp in four is heap-only on a full re-import.

### Turn a catalogue off

Set `[catalogues.<name>] enabled = false`, or `[catalogues] enabled = false` for all of them
([keys](#catalogue-sections)), and restart the daemon. A catalogue that is off:

- starts no importer;
- leaves search at once: no search lists its cards, and the text index removes their documents
  at its next pass ([text index](#text-index));
- keeps its rows, so turning it back on doesn't import it again from the start. Its state row's
  phase is `disabled` while the daemon runs with it off, and a run it had under way resumes from
  its cursor when it is on again;
- joins no shared link, and shows nothing of it on any card: no `labels`, no `from`. At its
  first start with the catalogue off, the daemon gives the links built from its cards back to the
  fetch queue in the background, due at times spread over the next day, and takes its keys off
  every link, each label removal with a change line ([shared links](#shared-links-a-catalogue-holds)).
  Later starts find nothing left to do. A link a later check finds built from a catalogue that is
  off, or that was dropped, is given back the same way, its read spread over the next day.
- reads nothing for its cards: no Wikipedia summary is asked for a card built from it. With every
  catalogue off, the daemon's joins task gives the links back, then checks the queue every ten
  minutes (it finds nothing there), and the extras task reads nothing.

### Shared links a catalogue holds

A link someone references, or resolves with `POST /v1/resolve`, that a catalogue card holds gets
its card from the catalogue at once and is never fetched: it is catalogue-backed
(`resource.catalogue_card`). Pinakes looks for the card before every fetch too, so a link shared
before its catalogue was imported joins at its next refresh. A card holds a link:

1. as its own link (`uri`, normalized);
2. by the link's `url:` key, for a link people share for the item;
3. by a key the link names by itself, its own catalogue's card first:

| Link | Key |
| --- | --- |
| A MusicBrainz artist, release group, release or recording page | `mb:<entity>:<mbid>` |
| A MusicBrainz ISRC page (`musicbrainz.org/isrc/<ISRC>`) | `isrc:<ISRC>` |
| A Wikidata item page | `wikidata:Q…` |
| A DOI (`doi.org`, `doi:`) | `doi:` and the DOI in lower case |
| An arXiv page | `arxiv:<id>` |
| An OpenAlex work (`openalex.org/W…`, `/works/W…`) | `openalex:W…` |
| A crates.io, docs.rs or lib.rs crate page | `crate:<name>` |
| An F-Droid app page | `android:<package>` |
| A Flathub app page | `flathub:<id>` |
| A site's homepage (path `/`, no query) | `homepage:<host without www.>` |
| A PubMed article page (`pubmed.ncbi.nlm.nih.gov/<n>`) | `pmid:<n>` |
| A show's Apple Podcasts page (not an episode's) | `podcast:apple:<id>` |

Two cards of one catalogue that hold a link equally join neither, an event card among them too. A
card typed as an event (an `Event` or one of its subtypes, such as a festival) backs no link: the
next card that holds it does, or the link is fetched. A matching name or any other
metadata never joins. OpenStreetMap cards back no link: an OpenStreetMap link keeps its OSM
route, which reads an imported object from its card instead of Nominatim
([`osm` catalogue](sources/osm_catalogue.md)). A Podcast Index card backs no link either: a show's feed is fetched for its episodes, and
joins the card's group as a related link. An event (dated or not) and a calendar (the event
routes), a Pubky object, a `geo:` point,
an alias, a link that carries a credential and social content (a post, even a gone one; and before
its first read, any link of a source of people's own posts, such as Bluesky, Mastodon, Reddit or X)
are never built from a card and join no card's group. A page on an OpenStreetMap host may join a group but
is never built from a card.

A catalogue-backed card:

- is built from the card: its type, name, description (cut to fit a card; left out when it
  carries a credential), point and `labels`; `from` names the catalogue. Its first save writes a
  change line, queues its [also-on](#also-on) sources and groups it, as a fetched card's does; a
  later build writes a change line when a field it shows changed, and asks the also-on sources
  and grouping again only when more than its labels changed;
- carries the card's keys and `catalogue:<catalogue>:<id>`, so every link built from the card,
  and every link that is one of its related links (`related_uris`, such as an official website),
  shares a group. A related link keeps its own card (fetched, or built from another card such as
  its site's Tranco card). A link related to two cards of one catalogue joins neither's group;
  one group holds at most one card of each catalogue;
- takes its article's Wikipedia summary as its description, read in the background when it is
  built from a card or an article it has no summary of, and when it is opened more than 30 days
  after the last read (`catalogue_extras`). A summary that carries a credential, or that would not
  fit the card, is not taken;
- is built again when its card changes. When the card is removed upstream, or no longer holds
  the link, the link goes back to the fetch queue: it is due at once (within the hour while more
  than 1,000 join checks are due, a large removal) and keeps its card until the fetch replaces it.
  The references people made stay on the link.

A card names the links it holds when it is written: its own link, its `url:` keys, the canonical
link of each of its keys (`catalogue_key_links`: the musicbrainz.org entity and ISRC pages, wikidata.org, `doi:`,
arxiv.org abstract and PDF, openalex.org, crates.io, docs.rs, lib.rs (a crate's name with `-` and with `_`), F-Droid
(English) and Flathub pages, and a homepage with or without `www.`) and its related links. The card table's triggers
mark those links' rows in
`catalogue_join_due`, so a link shared before the import joins then; a background task checks
them, a hundred at a time, every second (every ten minutes while no catalogue is joined: every
catalogue off, or OpenStreetMap alone). Another spelling of a key's link joins at its next fetch. A card never
names a link that carries a credential, or a Pubky object: the importer refuses it. With every
catalogue off nothing is looked up.

| Metric | Says |
| --- | --- |
| `pinakes_catalogue_joins_total{catalogue,outcome}` | Links built (`built`), given back (`released`) and not built for a card that makes no valid card (`invalid`) |
| `pinakes_catalogue_homepage_reads_total{outcome}` | The Tranco catalogue's homepage reads by outcome: `ok` (a card), `robots` (a robots.txt on the way says no), `wall`, `not_html`, `status` (a 4xx), `busy` (a 429 or 5xx), `unreached`, `deferred` (the egress gave no turn within two seconds: nothing read) and `other` (a redirect off the site, or no name) ([Tranco](sources/tranco_catalogue.md)) |
| `pinakes_catalogue_homepage_reads_waiting` | Whether Tranco's homepage reads wait for a large catalogue's first or running full import (1) or not (0), as its last run found; `CatalogueHomepageReadsStalled` fires when no read has found a page for six hours while sites are due and this is 0 |
| `pinakes_catalogue_join_errors_total{stage}` | Failed joins (`referenced`, `before_fetch`, `drain`) and extras reads (`extras`), each tried again; `CatalogueJoinErrors` past ten joins an hour for an hour |
| `pinakes_catalogue_backlog{queue}`, `pinakes_catalogue_backlog_age_seconds{queue}` | Join checks (`joins`), extras reads (`extras`) and Tranco homepage reads (`homepages`) due now, and the oldest one's wait; `CatalogueJoinsBehind` past six hours of joins. Extras follow Wikipedia's pace (a request a second at most), so after a first import they lag for hours by design. Homepage reads wait for the large catalogues' first imports ([Tranco](sources/tranco_catalogue.md)) |
| `pinakes_catalogue_join_errors_total{stage="extras"}` | Also counts a summary Wikipedia didn't answer. A check that keeps failing backs off (to an hour for joins, a day for extras) and waits outside the backlog |
| `pinakes_catalogue_cards_total{catalogue,outcome}` | The cards each catalogue's writes read: `inserted`, `changed`, `unchanged` (stamped only), `rejected` (refused as invalid) and `removed`; `CatalogueCardsRejected` past a tenth refused in an hour, `CatalogueCardsRemoved` past a tenth of the catalogue removed in a day |

To see the queues: `SELECT count(*), count(*) FILTER (WHERE due_at<=now()) AS due, min(due_at),
max(attempts) FROM catalogue_join_due` (and the same over `catalogue_extras`, whose rows without a
`due_at` are reads done).

### Status and drop

```sh
pinakesd --config <path> catalogue status
pinakesd --config <path> catalogue drop <name>
pinakesd --config <path> catalogue import <name> [--from FILE|DIR|URL]… [--limit N] [--reads N] [--update [--since TIME]] [--force]
pinakesd --config <path> catalogue import <name> --request
```

`catalogue status` prints one line per catalogue: whether it is on, its phase, its cards, the
upstream version it read (a dump's date, a file's version, Tranco's list and sites read), when its
last full import and update finished, how current its cards are, a full import an update asked
for (with its reason) and why it waits, and its last error.
The last error stays after a later success; the metrics count every failure.

`catalogue drop <name>` deletes the catalogue's cards, 10,000 per transaction, and its state row,
so it starts over with a full import when it is turned on again. Then it deletes the catalogue's
directory (`catalogues.dir/<name>`, relative to the command's working directory, as the daemon's
is) and prints the directory it removed, or that there was none. It refuses:

- a catalogue that is on in the configuration it is given: switch it off in the daemon's file,
  restart the daemon, then drop;
- while a daemon runs the catalogue's importer, which holds the catalogue's advisory lock for as
  long as it runs. The drop takes the same lock, so it can run beside the daemon.

The drop watches its lock while it deletes: when the lock's database session ends (a database
restart), it stops, says the lock was lost and deletes nothing more, since an importer may hold
the lock by then. It deletes the cards and the state row first, then checks the lock once more
and removes the directory only while it still holds it. Run it again to finish.

`catalogue import <name>` runs one full import of the catalogue now, or with `--update` one
update, through the same code and state rows as the daemon's task: it takes the catalogue's lock
(refused while a daemon runs the catalogue's importer), waits for the large-import turn and the
disk as the task does (printing why), watches its lock, and prints the cards before and after and
the status table when it has finished; it checks its lock before each step and watches it while
it waits, and stops (the lock lost) once its session has ended (the cards each run wrote: new, changed, unchanged,
rejected as invalid, removed). A full import takes up one that was cut off, else begins a
new one, and clears an update refused for removing too much. `--from`, `--limit`, `--reads`, `--since` and
`--force` are the importer's own ([`musicbrainz`](sources/musicbrainz_catalogue.md#run-it-by-hand),
[Wikidata](sources/wikidata_catalogue.md#run-it-by-hand); the others [below](#run-an-import)). A failure is recorded as the
task records it, unless the lock's database session has ended by then: the command then says the
lock was lost and records nothing. `catalogue import <name> --request` needs no lock: it records that a full import
is asked for (`import_requested`). The process running the importer starts it after an update
under way, or at its next step (MusicBrainz: within the hour when idle); a full import under way
is the one asked for, and its end, or its abandonment, clears the request. The command says when no process holds the
catalogue.

`scripts/lock-waits` names these locks `catalogue <name>` and `large catalogue import`.
`catalogue drop tranco` also deletes Tranco's site list (`catalogue_site`).

### Run an import

For the catalogues that read files, `--from` reads another file than the official one (once per
registry for `software`):

| Catalogue | `--from` |
| --- | --- |
| `openalex` | A directory holding a snapshot's `data/jsonl/` tree (its `manifest.json` and works files), or another bucket's URL |
| `podcastindex` | The database archive (`.tgz`, a file or a URL), or the unpacked `podcastindex_feeds.db` |
| `software` | `REGISTRY=FILE\|URL`, once per registry replaced: `crates.io=` the dump, `f-droid=` its `entry.json` (the index next to it), `flathub=` the AppStream `.xml.gz` |
| `tranco` | A list file (`rank,site` lines) |

A `--from` URL is read over `https` (`http` only on a loopback address); one with a user name, a
password or a query is refused, and the error does not repeat it. `osm` takes a local file only.

Every importer asks its upstreams the same way: directly (proxy variables in the environment are
not used), with the operator's contact in the User-Agent, and following a redirect (at most 3) only
to a host its catalogue names. A rate limit or a server error (429, 5xx) is asked again up to six
times, waiting as its `Retry-After` says; one that asks for more than five minutes fails the run
(`rate_limited`), and that host is not asked again until then (a day at most). A broken download
resumes at the byte it reached, under `If-Range`: a file replaced meanwhile fails `dump_changed`,
one gone `dump_gone`, and a server that brings less than 8 MB in ten minutes of waiting fails
`download`.
Without `http.contact` the daemon starts no importer and `catalogue status` shows `needs
http.contact` for every catalogue alike; a daemon's run that finds none waits for it, as one short
of disk does. `catalogue import` refuses without it unless it reads only local `--from` files
(Wikidata and Tranco always ask upstream, so they are refused).

`--reads N` is Tranco's: at most N homepages are read. `--limit N` (a card limit) and `--force` (a
`--limit` below the cards stored) are MusicBrainz's and Wikidata's; every other catalogue refuses
them, `osm` included (its scope is `catalogues.osm.region`). `catalogue import tranco` reads homepages through an egress of its own, apart from a running daemon's per-host pace: stop the daemon (or switch Tranco off there) first. Without `http.contact` (or
`PINAKES__HTTP__CONTACT`) only local files are read, and Tranco not at all.

### Catalogue sample runs

`scripts/catalogue-sample NAME [--keep] [options]` checks a catalogue's importer end to end
against a small real sample read from the real source:

| NAME | What it reads | Its own options |
| --- | --- | --- |
| `musicbrainz` | The first megabytes of each file of the two newest dumps (160 MiB in all) | `--import-only` (the newest dump, no update), `--bytes ENTITY=N` (a file's bytes) |
| `wikidata` | The first records of the newest dump, then the real recent changes | `--limit N` (cards, default 20,000), `--window MINUTES` (the update's, default 20), `--report FILE` |
| `osm` | A Geofabrik extract a few days old, then the region's change files since | `--region R` (default `europe/liechtenstein`), `--pbf FILE`, `--days N` (default 3), `--port N` (default 18492), `--max-mb N` (one download's most, default 300) |
| `openalex`, `podcastindex`, `software`, `tranco` | At most about 300 MB of each source; Tranco at most 50 homepages | |

Each creates a throwaway database, imports the sample with `pinakesd catalogue import`, checks the
cards (counts, keys, labels, canonical links), starts a daemon on a private port to check that
`/v1/search` finds a card and that its shared links join it through `POST /v1/resolve`, runs an
update and checks that only changed rows were written, checks that the catalogue switched off
leaves search, then drops the database and the downloads (OSM's downloads stay in its directory).
It prints a JSON report (bytes read, cards, time, peak memory, database size).
`scripts/catalogue-sample NAME --help` describes a run. Every catalogue takes the same flag and
environment:

- `--keep` keeps the database and the downloads.
- `PINAKESD` names the binary (default `target/release/pinakesd`).
- `PGHOST` and `PGPORT` name the PostgreSQL server the database is created on.
- `SAMPLE_DB` names the database (default `catalogue_sample_NAME`); a run refuses one that exists,
  or a name of other than lower case letters, digits and `_`.
- `SAMPLE_DIR` is where the downloads and the daemon's files go (default a new temporary directory).
- `PINAKES__HTTP__CONTACT` is the operator's contact, sent in the User-Agent; when unset it is read
  from `~/.config/pinakes/contact.env`, and it is never printed. Wikidata, OSM and Tranco refuse to
  run without it.

### Importer rules

Each catalogue's importer runs as a background task of `pinakesd`, started only for a
catalogue that is on and only with `http.contact` set (every request names the operator's contact; without it the daemon
logs that the importer does not start). It follows these
rules:

| Rule | What the task does |
| --- | --- |
| One importer per catalogue | Holds the catalogue's advisory lock for as long as it runs, and watches it between runs too: when its database session ends while the task waits for its next run, the task takes the lock again at once (and logs it), keeping its wait. A second daemon that finds the lock held waits and logs it once, leaving the state row to the importing daemon. |
| What runs | A full import when none has finished, then an update once the importer's interval has passed since the last finished full import or update (a day; MusicBrainz instead reads its mirror's `LATEST` once an hour and updates when it names a newer dump; Wikidata every 15 minutes, and at once while more than an hour behind; OpenStreetMap looks for a new change file every hour, and runs a full import every 30 days, the planet every 90; Tranco at once while the last run left homepages due, ten minutes after one that stopped for a large import, else when the next site falls due, ten minutes to a day after the last run). An update that can't apply the changes (a change stream too far behind) ends without marking anything current and asks for a full import (`import_requested`), which then starts as any new full import does: it waits for the large-import turn and the disk, and holds no turn while it waits. |
| How current | Each run states the upstream time of the data it leaves (a dump's date, the last change applied) as `current_as_of`; the update lag counts from it. An update that finds nothing new finishes without moving it, so a source that stops publishing shows as lag; Wikidata's update, which reads the changes by time, moves it to the end of the window it read even when the window held none (Wikidata changes many times a minute: the update logs a warning for a window of ten minutes or more without one). Each catalogue may lag as far as its source's publication rhythm allows (below). |
| One large import at a time | A full import of Wikidata, MusicBrainz, OpenStreetMap or OpenAlex waits while another of them runs or waits to be resumed after a failure or a restart (one switched off doesn't count). When two wait to be resumed, the first in that order goes first. Updates and the small catalogues don't wait. |
| Free disk | Before a run starts or resumes, compares the free disk under `catalogues.dir/<name>` with what the run still needs there: its downloads and temporary files only (the cards' share of the database is sized apart, as it may be on another disk; OpenAlex, software and Tranco read streams and need none). While it is short, waits and checks again every ten minutes, saying why in `waiting` and in the log (`catalogue import waits`). Nothing is started half. |
| Writes | At most 1,000 cards per transaction; a card is rewritten only when its content changed. |
| Progress | Saved with each write, so a run cut off by a restart or a failure resumes there. A failed run is recorded (`failed`, `last_error`, the error count) and tried again ten minutes later; so is one whose importer panicked. A run whose catalogue lock's database session has ended stops, is logged and resumes once the task holds the lock again; nothing about it is written to the state row, which another daemon may hold by then. So is a failure the run meets once that session has ended (a database restart ends every session).  Where the next update starts (a replication sequence, a time) is kept from one run to the next. |
| Refusals | A run whose data can't be applied is refused ([below](#a-refused-dump-or-file)): it is recorded as failed once, its run and cursor go (freeing the large-import turn), no card is removed for it, and what it read is not read again until a newer version comes. |
| Requests | A finished full import clears `import_requested`, also one asked for while it ran. A refused full import clears it only when it ran as the one asked for: a request that came while it ran stands. |
| Failures | Stored and logged by class (`download`, `parse`, `disk`, `size` (over a cap), `empty` (a full import that read no record), `refused` (what a run read would remove too much, or failed too often), `turn_lost`, `shrunk`, `panic`, `timeout`, …), with the host of a fixed bulk file or a local path where the importer names one; a database error by its own text. Never the text of something an upstream sent. |
| Removals | At the end of a full import, removes the cards it didn't read: an item removed upstream leaves with the next import that no longer finds it. An update removes the items its source says are gone (for MusicBrainz, those the new dump no longer has). A full import that read no record at all is refused (`empty`, [below](#a-refused-dump-or-file)), a first import too; one that read fewer than half the cards the catalogue holds removes nothing: it fails (`shrunk`), keeps its cursor and is tried again, so a cut-off dump or an empty answer never empties the catalogue. It goes through when the importer says the drop is expected, the operator allows it (`catalogue allow-shrink`, [below](#a-full-import-that-shrinks-the-catalogue)), or the import's scope is narrower than the last finished one's (a lower `limit`, a higher `min_citations`; kept in `catalogue_state.scope`, else the limit the state's position records). `catalogue import --force` with a `--limit` below both the cards held and the stored limit (none stored: every card) says the drop is expected (without such a limit, `--force` leaves the floor in place). Another `region` is no narrower scope: it may be larger, so it needs the allowance. |
| Updates that read a new file | An update writes a card only when its content hash differs from the stored one: an unchanged row is never touched. Podcast Index's weekly database and each software registry's file are read again whole, and the stored cards the file no longer names are removed at the end. An OpenAlex update reads the snapshot's newer files and removes the works in them that no longer qualify, at its end. An update that would remove more than a tenth of a scope's cards (the catalogue, or one registry of `software`), and more than ten, is refused (`refused`, [below](#a-refused-dump-or-file)): nothing is removed. A full import that reads no record at all (no card written or kept, Tranco: no listed site) is refused (`empty`), a first import too, before the shrink check. Tranco removes a card when its homepage read says so or its site has been off the list for a month ([Tranco](sources/tranco_catalogue.md)). The updates that remove by change (OpenStreetMap's change files, Wikidata's changes, Tranco's runs) are refused the same way past a tenth of the cards (and ten). |

### Allowed update lag

`CatalogueUpdateLag` fires when a catalogue's cards are further behind upstream than its source's
publication rhythm allows (`pinakes_catalogue_max_update_lag_seconds`, fixed per catalogue):

| Catalogue | Upstream publishes | Allowed lag |
| --- | --- | --- |
| `wikidata` | A change stream | 2 days |
| `osm` | Daily change files | 2 days |
| `tranco` | A daily list | 2 days |
| `software` | Daily registry files | 2 days |
| `musicbrainz` | Dumps twice a week | 5 days |
| `podcastindex` | A weekly database | 9 days |
| `openalex` | A quarterly snapshot | 100 days |

A Wikidata import leaves the cards as current as its dump's date, already two or three days old
when the file is out (and older by the import's end), and the updates then catch up (`pinakes_catalogue_catching_up` is 1). The
alert holds back while they do, unless the lag is more than five days past the allowed lag: a
catch-up that stopped still alerts.

### A full import that shrinks the catalogue

A full import that read fewer than half the cards the catalogue holds (for software, also one
registry's: `shrunk (<registry>: …)`) fails as `shrunk`, removes
nothing and is tried again every ten minutes from its last cursor (`CatalogueImportFailed` fires;
`catalogue status` shows `shrunk`). The catalogue's cards stay as they were.

1. Check upstream: a cut-off or empty download, or a filter that let too little through, shows
   in the importer's log lines.
2. If upstream really holds that few items, run `pinakesd --config <path> catalogue allow-shrink
   <name>`. It is refused unless the full import under way has failed and its last error is
   `shrunk` (for software, `shrunk (<registry>: …)` too). The next try removes the cards it did not read, and the allowance is used up; a run
   that fails for any reason first takes it back.
3. A narrower scope (a lower `limit`, a higher `min_citations`) needs no allowance: its first full
   import removes the rest on its own. `catalogue import NAME --limit N --force` runs one such
   import by hand: `--force` passes the floor only for a `--limit` below both the cards held and
   the limit the cards were read with (or the whole catalogue). A full import with `--force` keeps the floor;
   `allow-shrink` lets a real shrink through. Another `region` needs the allowance, as it may be larger.

### A refused dump or file

A run refuses what it read, and removes no card for it, when it can't be applied: an update that
would remove more than a tenth of the cards (`refused`), a full import that read no record
(`empty`), a dump or file that is broken (MusicBrainz: `checksum`, `parse`, `archive`, …; Podcast
Index: `archive`, `size`, `database`, `schema`), a Wikidata dump that failed three times
(`refused`). Each importer's page says which apply to it. The run is recorded once
(`CatalogueImportFailed`, `last_error`), its run and cursor go, and the large-import turn is free
for the others. The position names what was refused (`refused`), with where it was read
(`refused_source`: null for the catalogue's own files, else what `--from` named) and the
`limit` it was read with (`refused_limit`); `catalogue status` shows `refused: <version>` in
`waiting`, and `pinakes_catalogue_refused` is 1 while a refusal of the daemon's own source and
`limit` stands (alert `CatalogueDumpRefused` after an hour).

The importer then reads nothing while that version is the newest: each try (an hour apart) asks
only for the version (a `HEAD`, the dump list, `LATEST`) and ends without a failure (Tranco reads
no homepage either until its next list; software reads the other registries). The refusal goes
when:

- upstream publishes a newer version: the next run reads it, and its position replaces the
  refusal;
- a full import is asked for (`pinakesd catalogue import <name> --request` beside the daemon, or
  `catalogue import <name>` with it stopped): it reads the refused version again, and removes
  the cards it does not read as any full import (Tranco's caps on a run's removals don't apply
  to it);
- you clear it: `pinakesd --config <path> catalogue clear-refusal <name>`, beside the daemon. The
  next run reads the refused version again, as an update when one is due (a checksum failure
  while the mirror was still copying a file, say). If it still can't be applied, it is refused
  again.

### A large import that keeps failing

A large catalogue's full import that fails is resumed every ten minutes, and keeps the
large-import turn meanwhile: the other large catalogues' full imports wait for it
(`pinakes_catalogue_waiting_for_turn`, alert `CatalogueTurnBlocked`). `catalogue status` names it
in the others' `waiting` and shows its `last_error`. Fix the cause, or switch it off and restart
(its cut-off import then no longer holds the turn; it resumes when it is on again), or drop it.

### An import that keeps waiting

An importer that waits says why in `waiting` (`pinakesd catalogue status`), and
`pinakes_catalogue_waiting_seconds{reason}` counts how long (alert `CatalogueWaiting`):

- `disk`: the run needs more free disk under `catalogues.dir/<name>` than there is (`waiting`
  names both). Free disk there, or move `catalogues.dir`; the run starts at its next check, within
  ten minutes.
- `other`: a setting the run needs, such as `http.contact` for OpenStreetMap: set it and restart.
- `turn`: another large catalogue's full import runs or waits to be resumed (`waiting` names it).
  Check that it moves (its progress lines, `catalogue status`); one that keeps failing is
  [a large import that keeps failing](#a-large-import-that-keeps-failing).

### A Wikidata import that keeps failing

An import that never starts shows `waiting` in `pinakesd catalogue status`: `needs http.contact`
(set it and restart), short disk, or the large-import turn. One that fails shows the class in
`last_error` ([the Wikidata catalogue](sources/wikidata_catalogue.md#what-it-reads)):

| `last_error` | What happened | What to do |
| --- | --- | --- |
| `disk (<path>)` | A work file could not be written (the disk is full, a link stands in its place) | Free disk, or remove what stands in the way. While free disk is short, before a run or before a download round (once the dump's size is known), the run waits instead (`waiting` says how much it needs) and goes on from its cursor |
| `download`, `timeout`, `connect`, `api`, `read`, `not_found`, `size`, `rate_limited (…)` | The dumps host or the API did not answer, answered an error, a `--from` URL is not there, an answer was too large, or a wait over 5 minutes was asked; a dump read under 8 MB in ten minutes is `download`. After `rate_limited` nothing is asked of that host until the time it asked for (a day at most) has passed: the tries meanwhile fail at once | Nothing: the run resumes from its cursor every ten minutes. An update that fails `api` at the same position for hours (the log names the item an answer leaves out): `pinakesd catalogue import wikidata --request` reads a newer dump and loses nothing; `pinakesd catalogue import wikidata --update --since <a time past it>` (with the daemon's Wikidata off) is quicker but skips every item's changes up to that time (the progress line's `at` gives the time of the page reached) |
| `turn_lost` | The large-import lock was lost while the import ran | Nothing: it resumes from its cursor |
| `cursor` | The stored cursor names no valid dump | `pinakesd catalogue drop wikidata`, then import again |
| `no_dump (…)` | None of the newest dated directories lists the full JSON dump | Nothing: tried again in ten minutes |
| `shrunk` | The import read under half the cards the catalogue holds; nothing was removed | [A full import that shrinks the catalogue](#a-full-import-that-shrinks-the-catalogue) |
| `checksum`, `dump_changed`, `dump_gone`, `dump_missing`, `label_files`, `bz2_…`, `bad_dump` | The files were no good: they were deleted, and the next try reads the newest dump from its start (a local `--from` file is kept, and the import fails the same way) | Nothing, unless it repeats; for a local file, `pinakesd catalogue drop wikidata`, then import the fixed file |
| `refused (dump … failed 3 times (…))` | The newest dump failed three times: it is refused ([a refused dump or file](#a-refused-dump-or-file)), the import waits for the next weekly dump, and the large-import turn is free meanwhile | Wait for the next dump, or `pinakesd catalogue clear-refusal wikidata` to try this one again |
| `empty (…)` | The dump gave no card: refused, as above | As above |
| `other_import_under_way` | `catalogue import --from` named another file than the import under way | Let the import under way finish, or drop the catalogue |

An update that leaves a changed item as it was, because its answer alone is over the API cap
(64 MB), logs it by name (`a changed item's answer is over the cap`), counts it
(`pinakes_catalogue_items_skipped_total`, `over_cap`) and goes on; alert `CatalogueItemsSkipped`
fires for a day. The card misses that change until the item changes again (under the cap) or a full
import reads it from a dump: `pinakesd catalogue import wikidata --request` asks for one, when
the item matters before the next import.

## Monitoring

Scrape `GET /metrics` on the admin listener. [ops/README.md](../ops/README.md) has every series, the scrape
setup, the alert rules (`ops/alerts.yml`) and the Grafana dashboard (`ops/grafana/pinakes.json`).
`scripts/check-ops` checks every metric they name against the registry, and runs `promtool check rules
--lint-fatal` and the rule tests (`ops/alerts.test.yml`) when it is installed.

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
| `CatalogueDumpRefused` | warning | `pinakesd catalogue status` (`waiting`: `refused: <version>`, for software `refused: <registry> <version>, …`, each registry refused; `last_error`); [a refused dump or file](#a-refused-dump-or-file), [MusicBrainz](sources/musicbrainz_catalogue.md#a-dump-that-is-refused), and each other catalogue's page. A refusal of the daemon's own source and `limit` that has stood an hour, until a newer version is applied, a full import (`--request`) finishes, or `catalogue clear-refusal` clears it. |
| `CatalogueImportFailed`, `CatalogueUpdateLag` | warning | `pinakesd catalogue status`; [importer rules](#importer-rules), [allowed update lag](#allowed-update-lag). Both only for catalogues that are on; `CatalogueImportFailed` also for a first import that fails or is refused, `CatalogueUpdateLag` only after a first finished import. A catalogue on without its importer (one with an importer but no `http.contact`) shows in `catalogue status` (`waiting`) and `pinakes_catalogue_needs_contact`. |
| `CatalogueTurnBlocked` | warning | [A large import that keeps failing](#a-large-import-that-keeps-failing). |
| `CatalogueWaiting` | warning | [An import that keeps waiting](#an-import-that-keeps-waiting): an importer has waited over six hours for free disk or a setting, or over a week for the large-import turn (`pinakes_catalogue_waiting_seconds`). |
| `CatalogueJoinsBehind` | warning | [Shared links a catalogue holds](#shared-links-a-catalogue-holds): the oldest join check has waited over six hours. |
| `CatalogueHomepageReadsStalled` | warning | [Tranco](sources/tranco_catalogue.md): no homepage read has found a page for six hours while sites are due (`pinakes_catalogue_backlog{queue="homepages"}` above 0) and the reads do not wait for a large import (`pinakes_catalogue_homepage_reads_waiting` 0): every read deferred, or every read failing. |
| `CatalogueItemsSkipped` | warning | [A Wikidata import that keeps failing](#a-wikidata-import-that-keeps-failing): an update left a changed item as it was (its answer over the API cap) within the last day. |
| `CatalogueJoinErrors` | warning | [Shared links a catalogue holds](#shared-links-a-catalogue-holds): more than ten joins an hour fail, for an hour. |
| `SearchIndexClosed`, `SearchIndexStalled` | warning | The [text index](#text-index) has not opened for 30 minutes (an unwritable `search.index_dir`, a failing build), or has not read its queue for 10 (counted from its open, so passes that fail from the start fire it too); the log says why. Text searches read Postgres alone, or miss newer changes. |
| `TextSearchBusy` | warning | [Text index](#text-index): over ten text searches an instance answered 503 in ten minutes, for ten minutes. Raise `limits.search_index_searches` on a host with more cores, or find what makes searches slow. |
| `CatalogueCardsRejected`, `CatalogueCardsRemoved` | warning | [Importer rules](#importer-rules): a catalogue's runs rejected over a tenth of the cards they read in an hour (a dump format change, an importer bug: the debug log names the rejected cards), or removed over a tenth of the catalogue in a day (check upstream; a full import under half its cards fails instead). |

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
