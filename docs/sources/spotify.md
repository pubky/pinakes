# `spotify`: Spotify's own barcodes and ISRCs

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`spotify` (only with the operator's Spotify app credentials, `[spotify] client_id` and
`client_secret` or `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`,
[operations](#operations)): a Spotify album's barcode (`external_ids.upc`) and a track's ISRC
(`external_ids.isrc`) from Spotify's Web API, as the keys `upc:…` and `isrc:…` (`via`
`spotify`) and as facts, and no links; `musicbrainz`, `deezer` and `itunes` then find the same
album or track by them. An ISRC MusicBrainz gives to several recordings that agree is those
recordings' as one, keyed by the ISRC and the MBID that stands for them; one on recordings that disagree is decided
by the track's duration (`duration_ms`), the one recording within 3 s of it keyed by its MBID,
else the ISRC a fact only ([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)). A
Spotify link Spotify does not know (404)
gives nothing. Artists, playlists, episodes and shows are not read here.

## Another service's track on Spotify

The lookup the other way, a Spotify track for another service's track by its ISRC, is the
`spotify_isrc` source (its own page in this directory), with the same credentials and pace.

## Without Spotify's API

A Spotify link can still join when this source gives nothing: no credentials, a refusal (an
app owner without Premium, [below](#an-app-owner-without-premium)), or an outage. Two sources
look its URL up exactly, never by title or artist, and neither needs this one.
- **`musicbrainz`** asks MusicBrainz what it relates the link's URL to, and
  that lookup starts from no Spotify key. Sources run in a fixed order and see the keys the ones
  before them found, but a URL relation needs none of them, and a failing source
  costs only its own rows: the others' run and are kept.
  - **What MusicBrainz stores:** the one spelling `https://open.spotify.com/{kind}/{id}` (its
    URL cleanup folds the others), for a track (a recording, or an episode), an album (a
    release), an artist, and a show (a series). It takes no playlists. Every spelling Pinakes
    folds is that URL ([aliases](../api.md#aliases)), and that is the URL asked.
  - **Links stored before their spelling folded** (the embedded player's: an older build kept
    each as a link of its own) become aliases at `pinakesd reextract web media`, which selects
    the routes they are stored on. `open.spotify.com/embed/…` is on `media` (`web` with
    `[cards.media] enabled = false`), and `embed.spotify.com/?uri=…` on `web`, so
    `reextract web` alone leaves the first kind. `reextract all` covers both.
  - **Coverage is partial:** only links an editor added. For example, `…/track/6LgJvl0Xdtc73RJ1mmpotq` is
    related to the recording "Paranoid Android" (9f9cf187-d6f9-437f-9d98-d59cdbd52757), and
    `…/album/6dVIqQ8qmQ5GBnJ9shOYGE` to the release "OK Computer"; but
    `…/track/7tFiyTwD0nx5a1eklYtX2J` (the example above) is related to nothing (all three
    checked on 30 October 2026).
  - **What it gives:** a related link takes that recording's, release's or artist's keys
    (`mb:recording:…`, its ISRCs, …) and joins its group as any music link does. An album
    MusicBrainz does not relate by URL finds no release by barcode: only Spotify's own record
    has the barcode.
- **`wikidata`** asks for the one Wikidata item that carries the link's Spotify ID
  (P2207 for a track, P2205 for an album, P1902 for an artist). That is exact too, also needs no
  Spotify key, and covers only what Wikidata's editors entered.
- **Episodes and shows** are not music links here: this source reads none, so nothing falls
  back. A show's Spotify link can join its podcast through the show's Wikidata item
  (`podcasts`).
- **No Odesli (song.link).** Its API documentation says: "The entire v1-alpha.1 API namespace
  is deprecated and will be retired on July 31, 2026", and "We have stopped giving out API keys"
  (access only by allowlisting). Asked without a key on 30 October 2026, it answers 401
  `{"code":"PUBLIC_API_ACCESS_DEPRECATED"}`. Pinakes asks it nothing.

While this source is on, [`editions`](editions.md) asks the Web API which album a Spotify track a
card may show is on (`/v1/tracks/{id}`); where the API does not answer (no credentials, or
credentials it refuses, as for an app whose owner has no premium plan), and while
`[also_on] spotify_pages` is on (`true` by default), the track's public page, once an hour at
most, for the album it names (its `music:album`); a card's own Spotify track, a link someone
referenced, is read so whatever the switch says. Never an album page.

## Operations

`spotify` costs a token request when its token expires (a minute before Spotify's
`expires_in`, an hour), and per Spotify album or track one request (`/v1/albums/{id}`,
`/v1/tracks/{id}`), shared for an hour; a track's ISRC also asks MusicBrainz whether several
recordings have it (usually cached from `service`). Spotify's API is paced at 2 requests/s
(Spotify publishes no figure; it limits by a rolling 30-second window).

Spotify is asked only with a Spotify app's client ID and secret (developer.spotify.com; the
client-credentials grant: no user signs in, only the public catalogue is read; the app's owner
needs Spotify Premium, [below](#an-app-owner-without-premium)):
`PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`, the recommended form (one
underscore: a build without the source ignores them), or `[spotify] client_id` and
`client_secret`. Without both the `spotify` source does not run; credentials removed later leave
the rows it stored until they expire, as `enabled = false` does. The pair and the token never
appear in logs, the operator info, errors or debug output. Refused credentials (the token
endpoint refuses them, the API answers 403, or it rejects a token under a minute old) are logged
(`Spotify refused the client credentials`) and counted in
`pinakes_also_on_service_errors_total{service="spotify"}` (class `refused`, `forbidden` or
`rejected`) and in `pinakes_source_token_refusals_total{source="spotify"}`, which the
`ProviderKeyRefused` alert reads; the Spotify links read meanwhile keep what Spotify gave them before and retry later
(`spotify_auth`, after the five-minute hold), and Spotify is asked again when the hold ends. A
rate limit or an outage (429, 5xx) is waited out: no Spotify request before the time it asked
for (`Retry-After`, a minute to an hour) has passed, and the links retry after it
(`spotify_busy`). Spotify's answers are cached by the credential pair, not the token, so a new
token keeps them; a 401 or 403 is never cached. The token and refusal handling is TIDAL's
(`also_on/client_credentials.rs`), one slot per service and pair.

### An app owner without Premium

A Spotify app in Development Mode works only while the Spotify account that owns it (the one
that created it on developer.spotify.com) has an active Premium subscription
([Spotify's February 2026 migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)).
Without it, the token endpoint still issues a token, but every API request answers 403 with
`Active premium subscription required for the owner of the app.` For Pinakes that is refused
credentials (above):
- the log line `Spotify refused the client credentials` with `status` 403 and `class`
  `forbidden`, `pinakes_also_on_service_errors_total{service="spotify",class="forbidden"}`, and
  `pinakes_source_token_refusals_total{source="spotify"}` rising, so `ProviderKeyRefused` fires;
- the Spotify links read meanwhile keep what Spotify gave them before and wait with
  `spotify_auth` in `also_on_dirty.last_error`;
- the refusal is held five minutes, and Spotify is asked nothing while it holds, so it gets one
  token request and one API request at most every five minutes, each answered 403 again;
- each link's own retry is a failed also-on run's: `limits.also_on_retry_secs` (five minutes)
  after its first refusal, doubling with each one after, up to an hour.

The remedy is the owner's: take out or renew Premium on the account that owns the app. Spotify
then answers the same app again, with no new credentials and no restart. Pinakes asks it at the
first link retry due after the hold has ended, and that retry is the link's own: five minutes
after a link's first refusal, then 10, 20 and 40 minutes, and an hour after its fifth and each
one after. To ask for every link at once, run `pinakesd reextract also-on` with the daemon
stopped (it queues every resource's also-on lookups, not only Spotify's). Until then, to stop
asking, remove the credentials or turn the source off (`[also_on.sources.spotify] enabled = false`).
