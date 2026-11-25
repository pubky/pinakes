# `tidal`: TIDAL's own barcodes and ISRCs

This source runs on TIDAL album and track links. It reads the album's barcode or the track's ISRC
from TIDAL's developer API, so that other sources can find the same album or track by it. It is off
without the operator's TIDAL credentials.

| | |
| --- | --- |
| Source | `tidal`, also-on source |
| Runs on | TIDAL album and track links; not artists, videos or playlists |
| Asks | TIDAL: `GET https://openapi.tidal.com/v2/albums/{id}` or `/tracks/{id}`, with `countryCode`, and `POST https://auth.tidal.com/v1/oauth2/token` for a token; MusicBrainz for a track's ISRC |
| Credentials | Required: a TIDAL app's client ID and secret, `[tidal] client_id` and `client_secret`, or `PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`; off without both |
| Rate | At most 2 requests per second on `openapi.tidal.com`; 1 by default, as `limits.host_requests_per_second` is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | `upc:…` (albums), `isrc:…` and `mb:recording:…` (tracks) |
| Links | None |
| Facts | `upc` (albums), `isrc` (tracks) |
| Switch | `[also_on.sources.tidal] enabled`, on by default |

## What it does

1. For an album, the source reads `barcodeId`. For a track, it reads `isrc` and the track's length.
2. An answer counts only when it is the item asked for. An item TIDAL does not have, or does not
   offer in the configured country, gives nothing.
3. It records the value as a key (`via` `tidal`) and as a fact.

The sources that run after it use those keys. `musicbrainz` and `deezer` look the same
album or track up by them, and `itunes` lists a possibly same Apple Music album.

The source reads TIDAL's own links only. `tidal_isrc` finds another service's track
or album on TIDAL, with the same credentials.

### Country

`[tidal] country` is the country whose catalogue is read, `US` by default. It is an ISO 3166-1
alpha-2 code in upper case, and any other value stops startup.

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

One request per album or track. When a batch of due links holds two or more albums, or two or more
tracks, one request asks for up to 20 of them (`filter[id]`), without those that have a cached
answer. When a batch fails, the links ask alone. A track's ISRC also asks MusicBrainz, usually from
the cache because `service` asked it before.

A token is requested when the last one is within a minute of its expiry (`expires_in`). Answers are
shared for `limits.discovery_cache_secs` (an hour by default). They are cached by credential pair,
not by token, so a new token keeps them. A 401 or 403 is never cached.

### Refused credentials

TIDAL refuses the credentials when its token endpoint refuses them, when the API answers 403, or
when the API rejects (401) a token under a minute old. The refusal is:

- logged as `TIDAL refused the client credentials`;
- counted in `pinakes_also_on_service_errors_total{service="tidal"}`, with the class `refused`,
  `forbidden` or `rejected`;
- counted in `pinakes_source_token_refusals_total{source="tidal"}`, which the `ProviderKeyRefused`
  alert reads.

Pinakes asks TIDAL nothing for `limits.also_on_refusal_hold_secs` (300 seconds by default).
Meanwhile, TIDAL links keep what TIDAL gave them before. The source is passed over as one without
credentials: the run does not fail, and the link is read again at its usual time, or at once after
`pinakesd reextract also-on`.

A token that is rejected later in its life is dropped, and the lookup is retried with a new one.

### Rate limits and outages

After a 429 or a 5xx (or a 408 from the token endpoint), Pinakes sends TIDAL no request, batched or
single, until the time TIDAL named has passed (`Retry-After`, held between a minute and an hour).
The links retry after it (`tidal_busy`).

The client ID, the secret and the token never appear in logs, the operator info, errors or debug
output.

## Turn it off

```toml
[also_on.sources.tidal]
enabled = false
```

The rows it stored stop showing at the next start, and expire as usual. TIDAL links keep their own
cards, the keys other sources found, and the links MusicBrainz and Wikidata relate to them.

Removing the credentials is different: the source stops asking TIDAL, but the rows it stored keep
showing until they expire.
