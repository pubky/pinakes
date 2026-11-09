# `tidal`: TIDAL's own barcodes and ISRCs

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`tidal` (only with the operator's TIDAL client ID and secret, `[tidal] client_id` and
`client_secret` or `PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`,
[operations](#operations)): a TIDAL album's barcode and a track's ISRC from TIDAL's
own API, as the keys `upc:…` and `isrc:…` (`via` `tidal`) and as facts, and no links;
`musicbrainz`, `deezer` and `itunes` then find the
same album or track by them. An ISRC MusicBrainz gives to several recordings that agree is those
recordings' as one, keyed by the ISRC and the MBID that stands for them; one on recordings that disagree is decided by
the track's duration, the one recording within 3 s of it keyed by its MBID, else the ISRC a fact
only ([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)). TIDAL's catalogue is read in one country (`[tidal] country`,
`US`); an item TIDAL does not offer there gives nothing. TIDAL's answers are cached per
credential pair, so a new token keeps them; a 401 or 403 is never cached, and refused
credentials are asked again after five minutes.

## No lookup by ISRC

TIDAL's API can find a track by ISRC (`GET /v2/tracks?filter[isrc]=…`), but Pinakes does not ask
it to: such a lookup would only put a TIDAL link on a card beside other services' links, and
TIDAL's Developer Guidelines (version 3.0, 6 May 2024) list "Integrating TIDAL content with
content or streams from another service" among the offerings that are "strictly prohibited unless
you have TIDAL's express written approval", while its Design Guidelines (version 3.0) say not to
"display TIDAL content next to content from similar services". Its Developer Terms (version 3.0)
also allow TIDAL content only "the temporary caching of metadata and cover art" and forbid stored
"compilations or databases of TIDAL Content". A TIDAL track reaches a card as it does today: a
link someone tagged, MusicBrainz's or Wikidata's relations.

## Operations

`tidal` costs a token request when its token expires (a minute before TIDAL's `expires_in`),
and per TIDAL album or track one request, or a share of one: a batch of links asks for up to 20
albums or 20 tracks at once (`filter[id]`); a track's ISRC also asks MusicBrainz whether several
recordings have it (usually cached from `service`). TIDAL's API is paced at 2 requests/s (TIDAL
publishes no figure).

TIDAL is asked only with a TIDAL developer app's client ID and secret (developer.tidal.com):
`PINAKES_TIDAL_CLIENT_ID` and `PINAKES_TIDAL_CLIENT_SECRET`, the recommended form (one
underscore: a build without the source ignores them, where `PINAKES__TIDAL__CLIENT_ID` stops it),
or `[tidal] client_id` and `client_secret`. Without both the `tidal` source does not run;
credentials removed later leave the rows it stored until they expire, as `enabled = false` does.
`[tidal] country` (`US`, an ISO 3166-1 alpha-2 code in upper case; another value stops startup)
is the country whose catalogue is read. The pair
and the token never appear in logs, the operator info, errors or debug output. Refused
credentials (the token endpoint refuses them, the API answers 403, or it rejects a token under a
minute old) are logged (`TIDAL refused the client credentials`) and counted in
`pinakes_also_on_service_errors_total{service="tidal"}` (class `refused`, `forbidden` or
`rejected`); the TIDAL links read meanwhile keep what TIDAL gave them before and retry later
(`tidal_auth`, after the five-minute hold, then backing off to hourly), and TIDAL is asked again
when the hold ends. A rate limit
or an outage (429, 5xx) is waited out: no TIDAL request, batched or single, before the time it
asked for (`Retry-After`, a minute to an hour) has passed, and the links retry after it
(`tidal_busy`; a failure the cache replays meanwhile reports the same). TIDAL's answers are
cached by the credential pair, not the token, so a new token keeps them, and a batch asks only
for the items that have no cached answer.
