# API keys and credentials

Pinakes needs one secret to run: the PostgreSQL connection URL. Every provider credential is
optional. Without one, only that provider's part is missing: its links get ordinary web cards,
or its enrichment is left out, and the rest of the index works. A source's switch still applies
when its key is set.

See [deployment](deployment.md) for where the daemon runs, [operations](operations.md) for every
setting, and the [source catalogue](sources/README.md) for each source.

## Store and load secrets

Keep secrets in the environment, never in the TOML file, on the command line, in git, in a shared
URL, a support ticket or a log.

1. Copy [`.env.example`](../.env.example) to a file outside the checkout, owned by the service
   user, with mode `0600`.
2. Fill in the values you use. Every value in the example is a placeholder.
3. Load it with systemd `EnvironmentFile=`, `docker run --env-file`, or your runtime's secret
   store. `pinakesd` does not read a dotenv file itself. From a shell, load it with
   `set -a; . /path/to/pinakes.env; set +a`.

Don't enable shell tracing or print the environment while the file is loaded. Treat the database
URL as a secret too.

To rotate a credential, change it at the provider, update the environment file and restart the
daemon. Keys are read only at startup.

## How variables are read

Two forms set a credential:

- `PINAKES__SECTION__KEY` (two underscores between parts) sets any key of the TOML file, such as
  `PINAKES__SOURCES__STACK_EXCHANGE__KEY` for `[sources.stack_exchange] key`. It wins over the
  file. An unknown key stops startup.
- A variable a source names for itself (`PINAKES_NAME`, one underscore, or `TMDB_API_KEY`). A
  build without that source ignores it.

A named variable meets the file in one of three ways:

| Variables | Effect |
| --- | --- |
| `PINAKES_SPOTIFY_CLIENT_*`, `PINAKES_TIDAL_CLIENT_*`, `TMDB_API_KEY` | Replaces the value in the file and in the `PINAKES__…` form |
| `PINAKES_GITHUB_TOKEN`, `PINAKES_COURTLISTENER_TOKEN`, `PINAKES_DISCOGS_CONSUMER_*` | Fills the value only when the file and the `PINAKES__…` form leave it unset |
| `PINAKES_NEXUS_AUTHORIZATION` | Replaces the value; set empty, it clears the value the file holds |

Every other named variable that is set but empty is ignored. Don't keep competing values in the
file and the environment.

Pinakes keeps every credential out of logs, errors, `Debug` output and `/v1/info`.

## Instance settings

| Variable | Purpose | Notes |
| --- | --- | --- |
| `PINAKES__DATABASE_URL` | The PostgreSQL connection URL, password included | Required. A peer-authenticated Unix socket needs no password (`postgresql:///pinakes?host=/var/run/postgresql`). See [database](operations.md#database). |
| `PINAKES__ADMIN_TOKEN` | The bearer token for every admin route (metrics, operator info, stored documents, curation) | Required when `admin_listen` is not a loopback address; startup stops without it. On loopback the daemon starts and logs a warning. An empty token is refused, so leave the line commented until it has a value. Use a long random string. |
| `PINAKES_NEXUS_AUTHORIZATION` | The whole `Authorization` header sent to a protected Nexus, such as `Bearer …` | Sent on every request to `nexus_url` and to no other host. Never put credentials in `nexus_url`. Leave it unset for an open Nexus. |
| `PINAKES__OSM__NOMINATIM_URL` | A hosted Nominatim's base URL that carries its key | The key must be in the URL's path: a URL with user info, a query or a fragment is refused at startup. Pinakes shows the URL as `[REDACTED]`. A self-hosted Nominatim's URL is no secret and can stay in the TOML file. See [Nominatim](deployment.md#nominatim). |
| `PINAKES__HTTP__CONTACT` | The operator's contact address in the User-Agent | Not a secret. Without it, SEC EDGAR links wait and are tried again every hour. |

## Optional provider credentials

These are all the provider credentials Pinakes reads. Request read-only access to public data
only; Pinakes needs no user sign-in, user scopes or write access from any of them.

| Provider | Variables | Config keys | Used by |
| --- | --- | --- | --- |
| Spotify | `PINAKES_SPOTIFY_CLIENT_ID`, `PINAKES_SPOTIFY_CLIENT_SECRET` | `[spotify] client_id`, `client_secret` | `spotify`, `spotify_isrc`; `itunes_song` and `editions` for a Spotify track |
| TIDAL | `PINAKES_TIDAL_CLIENT_ID`, `PINAKES_TIDAL_CLIENT_SECRET` | `[tidal] client_id`, `client_secret` | `tidal`, `tidal_isrc`; `itunes_song`, `youtube_match` and `editions` for a TIDAL track |
| TMDB | `TMDB_API_KEY` | `[tmdb] api_key` | `tmdb` ([TMDB](#tmdb)) |
| Discogs | `PINAKES__ALSO_ON__SOURCES__DISCOGS__TOKEN`, or `PINAKES_DISCOGS_CONSUMER_KEY` and `PINAKES_DISCOGS_CONSUMER_SECRET` | `[also_on.sources.discogs] token`, or `key` and `secret` | The `discogs` card source and its also-on half |
| Podcast Index | `PINAKES__PODCAST_INDEX__KEY`, `PINAKES__PODCAST_INDEX__SECRET` | `[podcast_index] key`, `secret` | The `podcast_index` card source and the `podcasts` also-on source |
| GitHub | `PINAKES_GITHUB_TOKEN` | `[sources.github] token` | `github` |
| CourtListener | `PINAKES_COURTLISTENER_TOKEN` | `[sources.courtlistener] token` | `courtlistener` |
| Stack Exchange | `PINAKES__SOURCES__STACK_EXCHANGE__KEY` | `[sources.stack_exchange] key` | `stack_exchange` |
| Ticketmaster | `PINAKES__EVENT__ADAPTERS__PLATFORMS__TICKETMASTER__KEY` | `[event.adapters.platforms.ticketmaster] key` | The Ticketmaster event platform |

What changes with each credential:

| Provider | Without it | With it |
| --- | --- | --- |
| Spotify | The sources that ask Spotify are off. Spotify links still get their ordinary cards. | The sources ask the Web API with client credentials. Both values are needed. An app in Spotify's development mode answers only while the account that owns it has Premium; without it, every request gets 403. |
| TIDAL | The sources that ask TIDAL are off. TIDAL links still get their ordinary cards. | The sources ask TIDAL's developer API with client credentials. Both values are needed. `[tidal] country` (default `US`) sets the catalogue's country. |
| TMDB | `tmdb` is off: no request, and its stored values are hidden. | Film and series cards without an image or description take TMDB's poster and overview. |
| Discogs | Discogs cards and the also-on lookup work without images. | Cards show images. The token wins when both a token and a consumer pair are set. Pinakes paces `api.discogs.com` at 25 requests a minute either way. |
| Podcast Index | `podcast_index` declines and Podcast Index pages get web cards. `podcasts` never asks Podcast Index. | Pinakes signs API requests with the pair. Both values are needed. |
| GitHub | `github` is off; repository links get web cards. | Repository links get cards from the REST API. |
| CourtListener | `courtlistener` is off; opinion links get web cards, which may meet CourtListener's challenge page. | Opinion links get cards from its search API. |
| Stack Exchange | The source runs on the keyless quota, 300 requests a day for the host's address, paced to one request every 288 seconds. | The quota is 10,000 requests a day, paced to match. The key identifies an app and is not a secret, but Pinakes keeps it out of logs as it does a token. |
| Ticketmaster | The platform is off (`missing API key`); its links get web cards. | Event links are read from the Discovery API. The key is sent as the `apikey` query parameter on the wire only. |

A key never turns on a source or platform that is switched off.

### Where to get them

- **Spotify:** create an app in the [Spotify developer dashboard](https://developer.spotify.com/dashboard).
  Pinakes uses the [client credentials flow](https://developer.spotify.com/documentation/web-api/tutorials/client-credentials-flow).
- **TIDAL:** register an app in the [TIDAL developer portal](https://developer.tidal.com/).
- **TMDB:** create an API key in your account's [API settings](https://www.themoviedb.org/settings/api)
  and use its **API Read Access Token**.
- **Discogs:** in [Discogs developer settings](https://www.discogs.com/settings/developers), create
  an application for a consumer key and secret, or generate a personal access token. A personal
  token carries its holder's account access, so prefer the consumer pair.
- **Podcast Index:** request a key and secret at [api.podcastindex.org](https://api.podcastindex.org/signup).
- **GitHub:** create a [fine-grained token](https://github.com/settings/personal-access-tokens/new)
  with public-repository access and no added permissions, and give it an expiry.
- **CourtListener:** sign in and copy the token from the [API token page](https://www.courtlistener.com/profile/api-token/).
- **Stack Exchange:** [register an app on Stack Apps](https://stackapps.com/apps/oauth/register)
  and use its **Key**, not its client secret.
- **Ticketmaster:** request a Discovery API consumer key at [developer.ticketmaster.com](https://developer.ticketmaster.com/).

### Event platforms

The event platforms are listed in [`data/event-adapters.json`](../data/event-adapters.json). Only
the entries with `"requires_key": true` need a key; Ticketmaster is the only one. Set it at
`event.adapters.platforms.<id>.key`. A platform that needs a key is off until it has one (empty
means no key).

### Sources without a credential

Every other source needs no credential. You switch each one off in its own section
([switches](sources/README.md#switches)).

## TMDB

`TMDB_API_KEY` has no `PINAKES_` prefix. `[tmdb] api_key` in the file is read too, but keep the
key in the environment. With it, the `tmdb` source adds TMDB's poster and overview to a film or
series card that has no image or description of its own. It finds the title by the card's IMDb ID
or Wikidata item alone, never by its name.

Pinakes accepts two forms:

| Form | How Pinakes sends it |
| --- | --- |
| **API Read Access Token** (recommended), a JSON Web Token starting with `eyJ` | As `Authorization: Bearer …` |
| v3 **API Key**, 32 hexadecimal digits | As the `api_key` query parameter, added on the wire only. Pinakes caches, logs and reports the request without it. |

A value of neither form stops startup; the error message never repeats the value.

TMDB is off when the key is unset or empty, or with `[tmdb] enabled = false`. Then Pinakes sends
no request, hides the stored rows, and drops TMDB values from cards at their next read.

While TMDB is on:

- Every TMDB value on a card is credited (`credits[]`, provider `tmdb`).
- `/v1/info` gives TMDB's notice ("This product uses the TMDB API but is not endorsed or certified
  by TMDB.") and logo for clients to show. With TMDB off, it names TMDB only while a stored card
  still holds a TMDB value.
- No TMDB value stays on a card more than 60 days after the last read that answered
  ([also on](operations.md#also-on)).
- A change line holding a TMDB value is deleted after 120 days at most. Startup refuses a
  `limits.change_retention_days` over 120 while TMDB is on.

## Check a key

After you set or change a key:

1. Restart the daemon with the environment file loaded.
2. Read a few cards that use the key. Check the cards themselves, not only process health.
3. Check private `/metrics`. Don't log the request headers or a URL that holds a key.

`pinakes_source_token_refusals_total{source}` counts each time a provider refused the configured
credential since the daemon started. A refusal is a 401, a 403 where the provider means a refused
key, or a refused token request; a 429 or 5xx never counts. Each source below shows `0` from
startup:

`courtlistener`, `discogs`, `github`, `osm` (a keyed Nominatim), `podcast_index`, `spotify`,
`ticketmaster`, `tidal`, `tmdb`.

The `ProviderKeyRefused` alert in [`ops/alerts.yml`](../ops/alerts.yml) fires while the count rose
in the last six hours. A rising count means the key is wrong, expired or revoked. Fix it and
restart; the count resets at startup.

GitHub, CourtListener, Discogs and Podcast Index stop sending a refused credential for an hour,
then send it again. Each new refusal counts and starts another hour. Meanwhile GitHub,
CourtListener and Podcast Index links get web cards, and Discogs cards come without images.

To apply a new key to cards already stored, stop the daemon and run
`pinakesd --config <path> reextract also-on` for also-on results, or
`pinakesd --config <path> reextract web official` for official-source cards, then start it again.
See [re-extraction](operations.md#re-extraction).
