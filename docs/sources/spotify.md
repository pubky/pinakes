# `spotify`: Spotify's own barcodes and ISRCs

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`spotify` (only with the operator's Spotify app credentials, `[spotify] client_id` and
`client_secret` or `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`,
[operations](#operations)): a Spotify album's barcode (`external_ids.upc`) and a track's ISRC
(`external_ids.isrc`) from Spotify's Web API, as the keys `upc:…` and `isrc:…` (`via`
`spotify`) and as facts, and no links; `musicbrainz`, `deezer` and `itunes` then find the same
album or track by them. An ISRC MusicBrainz gives to several recordings is decided by the
track's duration (`duration_ms`), the one recording within 3 s of it keyed by its MBID, else the
ISRC a fact only ([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)). A
Spotify link Spotify does not know (404)
gives nothing. Artists, playlists, episodes and shows are not read here.

For example, the track `https://open.spotify.com/track/7tFiyTwD0nx5a1eklYtX2J` ("Bohemian
Rhapsody - Remastered 2011") has the ISRC `GBUM71029604`, which MusicBrainz gives to one
recording: the track gets `isrc:GBUM71029604` and joins that recording's group.

## Operations

`spotify` costs a token request when its token expires (a minute before Spotify's
`expires_in`, an hour), and per Spotify album or track one request (`/v1/albums/{id}`,
`/v1/tracks/{id}`), shared for an hour; a track's ISRC also asks MusicBrainz whether several
recordings have it (usually cached from `service`). Spotify's API is paced at 2 requests/s
(Spotify publishes no figure; it limits by a rolling 30-second window).

Spotify is asked only with a Spotify app's client ID and secret (developer.spotify.com; the
client-credentials grant: no user signs in, only the public catalogue is read):
`PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`, the recommended form (one
underscore: a build without the source ignores them), or `[spotify] client_id` and
`client_secret`. Without both the `spotify` source does not run; credentials removed later leave
the rows it stored until they expire, as `enabled = false` does. The pair and the token never
appear in logs, the operator info, errors or debug output. Refused credentials (the token
endpoint refuses them, the API answers 403, or it rejects a token under a minute old) are logged
(`Spotify refused the client credentials`) and counted in
`pinakes_also_on_service_errors_total{service="spotify"}` (class `refused`, `forbidden` or
`rejected`); the Spotify links read meanwhile keep what Spotify gave them before and retry later
(`spotify_auth`, after the five-minute hold), and Spotify is asked again when the hold ends. A
rate limit or an outage (429, 5xx) is waited out: no Spotify request before the time it asked
for (`Retry-After`, a minute to an hour) has passed, and the links retry after it
(`spotify_busy`). Spotify's answers are cached by the credential pair, not the token, so a new
token keeps them; a 401 or 403 is never cached. The token and refusal handling is TIDAL's
(`also_on/client_credentials.rs`), one slot per service and pair.
