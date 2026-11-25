# `spotify`: Spotify's own barcodes and ISRCs

This source runs on Spotify album and track links. It reads the album's barcode or the track's ISRC
from Spotify's Web API, so that other sources can find the same album or track by it. It is off
without the operator's Spotify app credentials.

| | |
| --- | --- |
| Source | `spotify`, also-on source |
| Runs on | Spotify album and track links; not artists, playlists, episodes or shows |
| Asks | Spotify: `GET https://api.spotify.com/v1/albums/{id}` or `/tracks/{id}`, and `POST https://accounts.spotify.com/api/token` for a token; MusicBrainz for a track's ISRC |
| Credentials | Required: a Spotify app's client ID and secret, `[spotify] client_id` and `client_secret`, or `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`; off without both |
| Rate | At most 2 requests per second on `api.spotify.com`; 1 by default, as `limits.host_requests_per_second` is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | `upc:…` (albums), `isrc:…` and `mb:recording:…` (tracks) |
| Links | None |
| Facts | `upc` (albums), `isrc` (tracks) |
| Switch | `[also_on.sources.spotify] enabled`, on by default |

## What it does

1. For an album, the source reads `external_ids.upc`. For a track, it reads `external_ids.isrc` and
   the length (`duration_ms`).
2. An answer counts only when it is the object asked for. A link Spotify does not know (404) gives
   nothing.
3. It records the value as a key (`via` `spotify`) and as a fact.

The sources that run after it use those keys. `musicbrainz` and `deezer` look the same
album or track up by them, and `itunes` lists a possibly same Apple Music album.

The source reads Spotify's own links only. `spotify_isrc` finds another service's
track or album on Spotify, with the same credentials.

No user signs in: Pinakes uses the client-credentials grant and reads only the public catalogue.

### Without Spotify's API

A Spotify link can join other cards when this source gives nothing (no credentials, refused
credentials or an outage). These sources look its URL up exactly and need no Spotify credentials:

- **`musicbrainz`** asks MusicBrainz what it relates the link's URL to. MusicBrainz stores one
  spelling, `https://open.spotify.com/{kind}/{id}`. Every spelling Pinakes folds is that URL
  ([aliases](../api.md#aliases)), and that is the one asked. It finds only what an editor related.
  An album MusicBrainz does not relate by URL finds no release there, because only Spotify's own
  record has its barcode.
- **`wikidata`** asks for the one Wikidata item that carries the link's Spotify ID (P2207 for a
  track, P2205 for an album, P1902 for an artist).

### A track's album

While this source is on, `editions` asks the Web API which album a Spotify track a
card may show is on (`/v1/tracks/{id}`). Where the API does not answer (no credentials, or
credentials it refuses), `editions` reads the track's public page for the album it names
(`music:album`), if `[also_on] spotify_pages` is `true` (the default). A card's own Spotify track is
read that way whatever the switch says. Album pages are never read.

## Keys, links and facts

Barcodes are stored as GTIN-13 and ISRCs in upper case. A barcode is always a key.

A track's ISRC is a key according to what MusicBrainz knows about it
([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)):

- No recording, or exactly one, has it: the ISRC is the key.
- Several recordings have it and they agree: the keys are the ISRC and the MBID of the recording
  that stands for them.
- Several have it and they disagree: the one recording within 3 seconds of the track's length is
  keyed by its MBID, and the ISRC is a fact. When the length picks none, the ISRC is a fact only.

## Cost

One request per album or track. A track's ISRC also asks MusicBrainz, usually from the cache. A
token is requested when the last one is within a minute of its expiry (`expires_in`). Answers are
shared for `limits.discovery_cache_secs` (an hour by default). They are cached by credential pair,
not by token, so a new token keeps them. A 401 or 403 is never cached.

### Refused credentials

Spotify refuses the credentials when its token endpoint refuses them, when the API answers 403, or
when the API rejects (401) a token under a minute old. The refusal is:

- logged as `Spotify refused the client credentials`;
- counted in `pinakes_also_on_service_errors_total{service="spotify"}`, with the class `refused`,
  `forbidden` or `rejected`;
- counted in `pinakes_source_token_refusals_total{source="spotify"}`, which the
  `ProviderKeyRefused` alert reads.

Pinakes asks Spotify nothing for `limits.also_on_refusal_hold_secs` (300 seconds by default).
Meanwhile, Spotify links keep what Spotify gave them before. The source is passed over as one
without credentials: the run does not fail, the other sources' results are recorded, and the link
is read again at its usual time, or at once after `pinakesd reextract also-on`.

A token that is rejected later in its life is dropped, and the lookup is retried with a new one.

### Rate limits and outages

After a 429 or a 5xx (or a 408 from the token endpoint), Pinakes sends Spotify no request until the
time Spotify named has passed (`Retry-After`, held between a minute and an hour). The links retry
after it (`spotify_busy`).

The client ID, the secret and the token never appear in logs, the operator info, errors or debug
output.

### An app owner without Premium

An app in Spotify's development mode answers only while the account that owns it has an active
Premium subscription ([API keys](../api-keys.md)). Without it, the API answers every request with
403, which Pinakes treats as refused credentials (class `forbidden`). To restore the source, renew
Premium on the account that owns the app. Pinakes needs no new credentials and no restart: it asks
Spotify again once the hold has ended, when a link is next read. To ask for every link at once, run
`pinakesd reextract also-on` with the daemon stopped. It queues every resource's also-on lookups,
not only Spotify's.

## Turn it off

```toml
[also_on.sources.spotify]
enabled = false
```

The rows it stored stop showing at the next start, and expire as usual. Spotify links keep their own
cards and the keys other sources found. `editions` stops placing Spotify tracks by
their album.

Removing the credentials is different: the source stops asking Spotify, but the rows it stored keep
showing until they expire.
