# API keys and credentials

Pinakes needs a PostgreSQL connection, not a paid API bundle. Provider credentials are optional:
missing credentials remove that provider's enrichment, not the rest of the index. A web fallback
is best-effort; a provider's page may itself be walled. Source switches still apply when a key is set.
See [deployment](deployment.md), [operations](operations.md) and the [source catalogue](sources/README.md).

## Store and load secrets

Use environment variables only for secrets, never the TOML file, command-line arguments, git,
URLs shared with others, support tickets or logs. Copy [`.env.example`](../.env.example) into a
service-user-owned file outside the checkout, set mode `0600`, and inject it with systemd
`EnvironmentFile=` or your runtime's secret store. `pinakesd` does not load a dotenv file itself.
A shell-compatible file can be loaded before starting the daemon with `set -a; . /path/to/dev.env; set +a`.
Do not enable shell tracing or print the environment. Protect database URLs as secrets too.

`PINAKES__SECTION__KEY` overrides the corresponding TOML setting; unknown keys stop startup.
The single-underscore names below are deliberate. Spotify/TIDAL named variables override their
section values. GitHub/CourtListener short token names and Discogs consumer names fill only unset
settings; do not keep competing values in TOML or other environment variables. Empty named values
are ignored, except `PINAKES_NEXUS_AUTHORIZATION`, whose empty value clears authorization.
Rotate at the provider, update the protected environment and restart; never log a token to test it.

## Instance settings

| Environment variable | Purpose | Provisioning and minimum access |
| --- | --- | --- |
| `PINAKES__DATABASE_URL` | PostgreSQL connection | A database dedicated to Pinakes. Provision extensions and schema with a migration role; the runtime role needs only the application's database access, not cluster superuser. Peer-authenticated sockets need no password. See [database operations](operations.md#database). |
| `PINAKES__ADMIN_TOKEN` | Bearer authentication for admin routes, including metrics | Generate a long random token locally; required for non-loopback admin binds, recommended on loopback too (startup warns without one). Keep the listener private even with a token. |
| `PINAKES__OSM__NOMINATIM_URL` | A hosted Nominatim whose base URL carries its key | Only for a keyed hosted Nominatim (see [deployment](deployment.md#nominatim)); a self-hosted one's URL is no secret and may stay in the TOML file. The key must be in the URL's path: user info, a query or a fragment is refused. Pinakes logs a request by its path below the base alone and shows the URL as `[REDACTED]`. |
| `PINAKES_NEXUS_AUTHORIZATION` | Authorization for a protected Nexus | Obtain read access from the Nexus operator to the reference feed, reach lists/reach-set and resource batch view. Supply the entire header value, including `Bearer` or `Basic`. No redirect is followed; never put credentials in `nexus_url`. Omit for an unprotected Nexus. |
| `PINAKES__HTTP__CONTACT` | Contact in the identifying User-Agent; not a secret | An operator-controlled contact address. SEC EDGAR links wait without it; it is not a universal switch that disables every key-free source. |

## Optional provider credentials

These are all credentialed providers in the current runtime configuration and event-adapter
registry. Staging and production may share a provider application's API credentials only where
the provider's terms, app/deployment rules and combined rate allowance permit it. Pool and budget
their traffic explicitly; use separate apps/keys when required or when quota/revocation isolation
is needed. Share self-hosted read-only datasets similarly, with capacity and licence checks
([deployment](deployment.md#storage-model)), not writable databases or database/admin/Nexus secrets
across security boundaries. Each environment still injects its credentials from its own protected
secret store; sharing a provider allowance does not relax access controls or logging rules.
Provider dashboards can require account verification or approval; a successful signup is not a
promise of catalogue access. Request read-only public-data access, never publishing, ticket sales,
private repositories, user libraries or account administration.

| Environment variable(s) | Obtain credentials / least privilege | What works without them |
| --- | --- | --- |
| `PINAKES_SPOTIFY_CLIENT_ID`, `PINAKES_SPOTIFY_CLIENT_SECRET` | Create your application in the [Spotify developer dashboard](https://developer.spotify.com/dashboard). Pinakes uses [client credentials](https://developer.spotify.com/documentation/web-api/tutorials/client-credentials-flow), without user sign-in or user scopes. Development Mode requires the app-owner account to have Premium; see the [migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide). | No Spotify API ISRC/barcode enrichment. Spotify pages can still get web/media cards; exact MusicBrainz URL relations and Wikidata IDs can still join them, with partial coverage. [Source catalogue](sources/README.md#also-on). |
| `PINAKES_TIDAL_CLIENT_ID`, `PINAKES_TIDAL_CLIENT_SECRET` | Register your app in the [TIDAL developer portal](https://developer.tidal.com/); use client credentials for public catalogue reads, no user-account scopes. [Quick start](https://developer.tidal.com/documentation/api-sdk/api-sdk-quick-start). | No TIDAL API ISRC/barcode enrichment; page cards and independent MusicBrainz/Wikidata links remain. [Source catalogue](sources/README.md#also-on). |
| `TMDB_API_KEY` | Create a free API key in your TMDB account's [API settings](https://www.themoviedb.org/settings/api) and use its **API Read Access Token** (recommended; sent as `Authorization: Bearer`). The shorter v3 **API Key** also works ([TMDB](#tmdb)). Read-only; no user session. | No TMDB posters or overviews on film and series cards; their own page's image and text, and Commons images, remain. `/v1/info` names no TMDB attribution. [Source catalogue](sources/README.md#also-on). |
| `PINAKES__ALSO_ON__SOURCES__DISCOGS__TOKEN` **or** `PINAKES_DISCOGS_CONSUMER_KEY` plus `PINAKES_DISCOGS_CONSUMER_SECRET` | [Discogs developer settings](https://www.discogs.com/settings/developers): prefer an app's consumer pair, which grants catalogue images without identifying a user. A personal token is also supported but carries the token holder's account access; keep it private. Pinakes uses only public reads and needs no user OAuth grant. The token wins over the consumer pair. [Authentication reference](https://www.discogs.com/developers/#page:authentication). | Public Discogs records and barcode matching remain, without authenticated images and with the unauthenticated quota. [Source catalogue](sources/README.md). |
| `PINAKES__PODCAST_INDEX__KEY`, `PINAKES__PODCAST_INDEX__SECRET` | Request the pair from [Podcast Index signup](https://api.podcastindex.org/signup). Use catalogue reads only; Pinakes signs requests with the pair, not a user login. | No Podcast Index API cards or its also-on lookup; RSS/Atom, Apple Podcasts and Wikidata paths remain. [Cards and podcast joins](sources/README.md). |
| `PINAKES_GITHUB_TOKEN` (alternative: `PINAKES__SOURCES__GITHUB__TOKEN`) | [Create a fine-grained GitHub token](https://github.com/settings/personal-access-tokens/new) with public-repository access and no added permissions. Choose an expiry; do not grant private repositories or write access. [GitHub token guide](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens). | Repository links use the ordinary web resolver; the authenticated official API source is off. [Source catalogue](sources/README.md#sources). |
| `PINAKES_COURTLISTENER_TOKEN` (alternative: `PINAKES__SOURCES__COURTLISTENER__TOKEN`) | Register/sign in to CourtListener and obtain your token from the [profile API-token page](https://www.courtlistener.com/profile/api-token/), as directed by the [REST API guide](https://www.courtlistener.com/help/api/rest/). Only opinion search reads are used; no write operation or RECAP upload is needed. | The opinion API source is off; ordinary web resolution may encounter CourtListener's challenge page. 401/403 disables the source until restart. [Source catalogue](sources/README.md#sources). |
| `PINAKES__EVENT__ADAPTERS__PLATFORMS__TICKETMASTER__KEY` | [Ticketmaster developer signup](https://developer.ticketmaster.com/): request a Discovery API consumer key, not transaction or inventory-write access. [Discovery API](https://developer.ticketmaster.com/products-and-docs/apis/discovery-api/v2/). | Ticketmaster's keyed adapter is disabled; its URLs can use ordinary web metadata. |
| `PINAKES__EVENT__ADAPTERS__PLATFORMS__SKIDDLE__KEY` | Request an events API key through [Skiddle's API page](https://www.skiddle.com/api/). Read public event details only. | Skiddle's keyed adapter is disabled; web metadata remains best-effort. |
| `PINAKES__EVENT__ADAPTERS__PLATFORMS__BILLETTO_API__KEY` | In a Billetto organiser account, open **Integrate → Developers** and generate a key pair: [official instructions](https://api.billetto.com/docs/obtaining-an-api-key). Supply `key:secret` as the complete `Api-Keypair` header value. Use public event reads only; no organiser-write integration is needed. | The keyed Billetto API adapter is disabled. The separate public-page adapter has its own switch and is off by default; no challenge bypass is supplied. |
| `PINAKES__EVENT__ADAPTERS__PLATFORMS__EVENTFROG_API__KEY` | Contact [Eventfrog](https://eventfrog.net/) for partner API credentials. Supply the bearer credential **without** the `Bearer ` prefix (Pinakes adds it). Ask for event-read access only. No public self-service issuance flow is verified. | The keyed Eventfrog API adapter is disabled; independent public-page resolution remains best-effort. |
| `PINAKES__EVENT__ADAPTERS__PLATFORMS__GOOGLE_CALENDAR_API__KEY` | Enable Calendar API in a dedicated [Google Cloud project](https://console.cloud.google.com/apis/library/calendar-json.googleapis.com), then create an API key under Credentials. Restrict it to Calendar API and, where possible, the server's fixed egress IPs. [Google's credential guide](https://developers.google.com/workspace/guides/create-credentials#api-key). Public-calendar reads need neither OAuth nor a service-account key. | No Calendar API cursor feed; public ICS feeds still work when available. |

For the event adapters the configuration path is
`event.adapters.platforms.<id>.key`; all five full variable names are written out above.
Their switches, routes and capabilities are defined in
[`data/event-adapters.json`](../data/event-adapters.json), not inferred from a provider offering an API.
Keys never enable an otherwise disabled platform automatically.

## TMDB

`TMDB_API_KEY` (no `PINAKES_` prefix; `[tmdb] api_key` in the file is read too, but keep the key
in the environment) lets the `tmdb` source ([source catalogue](sources/README.md#also-on)) add TMDB's poster and overview to a
film or series card that has no image or description of its own, by the title's IMDb ID or
Wikidata item alone. Set it to the **API Read Access Token** from
[themoviedb.org/settings/api](https://www.themoviedb.org/settings/api): Pinakes sends it as a
header. A v3 **API Key** (32 hexadecimal digits) works too; TMDB takes that one only in the
query string, so Pinakes adds it to the request on the wire alone and caches, logs and reports
the request without it. A value of neither form stops startup (the message never repeats it).
Unset or empty, or with `[tmdb] enabled = false`, TMDB is off: no request, its stored rows
hidden, its values gone from cards at their next read.

TMDB's terms ask for attribution and limit caching: Pinakes credits every TMDB value on its card
(`credits[]`, provider `tmdb`), `/v1/info` gives TMDB's notice ("This product uses the TMDB API
but is not endorsed or certified by TMDB.") and logo for clients to show, and no TMDB value is
kept longer than 60 days after the last read that answered (TMDB allows six months;
[operations](operations.md#also-on)); with the key set, startup refuses a
`refresh.retention_days` over 120, so change lines keep no TMDB value longer either. Commercial use needs a separate agreement with TMDB.

## Quotas and operating rules

Provider quotas and Pinakes' configured pacing are different. The global per-host budget defaults
to one request/second, with lower service-specific defaults and optional operator overrides;
[operations](operations.md#also-on) describes how they combine. Do not treat an override as a
provider quota increase. Honour `Retry-After` and check your account's actual allowance before
raising rates. Multiple processes behind one public IP share upstream limits.

Verified provider examples: [MusicBrainz](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting)
allows one request/second per client IP; [public Nominatim](https://operations.osmfoundation.org/policies/nominatim/)
allows at most one request/second and is not the production geocoder.
[Spotify](https://developer.spotify.com/documentation/web-api/concepts/rate-limits) uses a rolling
30-second window rather than a published universal requests/second figure.
[Ticketmaster Discovery](https://developer.ticketmaster.com/products-and-docs/apis/discovery-api/v2/)
defaults to 5,000 calls/day and 5 requests/second. These are provider ceilings, not promised throughput.
For other keys use the provider/account quota, not an invented universal limit.

No key is needed for MusicBrainz, Wikidata, Open Library or iTunes lookup. They still require
identification, caching, attribution and provider-specific pacing; see their source pages.
GovInfo's supported path does not use an api.data.gov key. FRED, Congress.gov and Companies House
credentials are not current Pinakes configuration: do not provision unsupported variables.

**Keep the Deezer API off:** leave `[also_on.sources.deezer] enabled = false` (the default).
This stops Deezer API reads and hides that source's own stored results, not the Deezer website:
Deezer links from MusicBrainz, Wikidata or links people reference remain allowed.
Odesli/song.link is not implemented; it is not a configured fallback or another key to obtain.

Review current provider terms for the actual deployment; open-source code does not by itself
establish permission for every deployment. This guide makes no commercial-use or branding ruling.
Keep the attributions and OSM dump described in [operations](operations.md#deployment-and-egress).

## Verify after setup

Restart with the protected environment. Check private admin info and `/metrics` without logging
headers or request URLs containing keys; public `/v1/info` intentionally exposes no credentials.
Monitor `pinakes_source_token_refusals_total` (GitHub, CourtListener, Discogs) and
`pinakes_also_on_service_errors_total` (including Spotify/TIDAL) for refusals.
With the daemon stopped, `pinakesd --config <path> reextract also-on` queues fresh joins;
`reextract web official` refreshes stored official-source cards. Start the daemon again and
check representative cards, not just process health. See [re-extraction](operations.md#database)
for the queues and retention behaviour when a source is disabled.
