# `spotify_isrc`: another service's track or album on Spotify, by its ISRC or barcode

This source runs on tracks and albums that already have an ISRC or barcode key. It adds links to the
Spotify tracks or the Spotify album that carry exactly that value. It searches by the value alone,
never by a title or a name, and it is off without the operator's Spotify app credentials.

| | |
| --- | --- |
| Source | `spotify_isrc`, also-on source |
| Runs on | Another service's track or video with an `isrc:` key, another service's album or YouTube album playlist with a `upc:` key, and MusicBrainz recording, ISRC and release pages with such a key ([which cards](README.md#lookups-by-isrc-or-barcode)) |
| Asks | Spotify: `GET https://api.spotify.com/v1/search?q=isrc:{ISRC}&type=track&limit=10`, `GET …/v1/search?q=upc:{barcode}&type=album&limit=3`, and `GET …/v1/albums/{id}` to read an album back |
| Credentials | Required: the same Spotify app credentials as `spotify`, `[spotify] client_id` and `client_secret`, or `PINAKES_SPOTIFY_CLIENT_ID` and `PINAKES_SPOTIFY_CLIENT_SECRET`; off without both |
| Rate | At most 2 requests per second on `api.spotify.com`, shared with `spotify`; 1 by default, as `limits.host_requests_per_second` is lower |
| Rows live | 30 days; rows expire after 60 |
| Keys | None of the card's own |
| Links | `https://open.spotify.com/track/{id}` and `https://open.spotify.com/album/{id}`, exact, keyed by the ISRC or barcode |
| Facts | None |
| Switch | `[also_on.sources.spotify_isrc] enabled`, on by default |

## What it does

Spotify's own tracks and albums are not asked. The source uses only values that are keys: a shared
ISRC is a fact, and Spotify would answer with any of its recordings. It asks at most two values per
card. For a track, its own ISRC comes first, then the hubs' ISRCs.

The source runs after every source that keys a track's ISRC or an album's barcode, `deezer` among
them.

### Tracks

The Web API's search with its `isrc:` filter is an exact lookup. The source takes only the tracks
whose own ISRC is the value asked, at most 5 in all. Several answers are the recording on several
releases: `editions` places them, and a card shows one per service.

### Albums

1. The search's `upc:` filter names up to 3 candidate albums. A UPC-A (a GTIN-13 with a leading
   zero) is asked by its 12 digits, an EAN-13 whole.
2. Each candidate is read back with `GET /v1/albums/{id}`, the request `spotify` makes, because the
   search's albums carry no barcode. Only an album whose own `external_ids.upc`, as GTIN-13, is the
   key counts.
3. Exactly one such album becomes the link. The lookup gives nothing for the barcode when:
   - no candidate, or several, carry the barcode;
   - a candidate could not be read (a 404), since it might be the other album with the barcode;
   - the search found more albums than it lists (a `next` page, or a `total` above its items), since
     the rest might hide a second album with the barcode.

## Keys, links and facts

Each link is keyed by the ISRC or barcode that found it, so a tagged Spotify link that is one of
them joins the group. The source records no key for the card itself and no facts.

## Cost

A track costs one search per ISRC key, at most two. An album costs one search per barcode key, at
most two, and one read per album the search lists, at most three. The answers are shared for
`limits.discovery_cache_secs` (an hour by default) by credential pair, and go through the same token
slot as `spotify`'s.

Refused credentials (an app owner without Premium among them), rate limits and outages are handled
as on the `spotify` page. Each keeps the links an earlier run found.

A 403 on the read of one album belongs to that read, not to the credentials. The lookup is retried
after `limits.also_on_refusal_hold_secs` (`forbidden_request`), keeping its links, and every other
Spotify request goes on.

`pinakesd reextract also-on` asks for every stored track and album at once. The credentials and the
token never appear in logs, the operator info, errors or debug output.

## Turn it off

```toml
[also_on.sources.spotify_isrc]
enabled = false
```

The links it stored stop showing on cards at the next start, and expire as usual. `spotify` goes on
without it.
