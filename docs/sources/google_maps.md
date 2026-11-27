# Google Maps

Pinakes reads a place's name and point from a Google Maps link itself. It never asks Google's
pages, embeds or APIs. A short link is the one exception: Pinakes asks it for its redirect and
reads the target link. No key or account is needed.

| | |
| --- | --- |
| Source | `google_maps`, official source |
| Links | `google.com/maps/place/{name}/…`, `/maps/search/…`, `/maps/@{lat},{lon},{zoom}z`, `/maps?q=…`, `/maps?ll=…`, `maps.google.com/?q=…`, on any of Google's domains; short links `maps.app.goo.gl/{code}` and `goo.gl/maps/{code}` |
| Fetches | Nothing for a place link; `GET https://maps.app.goo.gl/{code}` (or `https://goo.gl/maps/{code}`) for a short link, without following its redirect |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `maps.app.goo.gl` and `goo.gl` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)); a place link's refresh makes no request |
| Card types | `Place` |
| Keys | None of its own. The `wikidata_places` lookup ([places](places.md)) reads the place's CID from a place link |
| Credit | None |
| Switch | `[sources.google_maps] enabled`, on by default |

## Links

The source reads links on `google.{domain}`, `www.google.{domain}` and `maps.google.{domain}`,
where `{domain}` is one of the domains Google lists at `https://www.google.com/supported_domains`
(`com`, `de`, `co.uk`, `com.au`, …). The path starts with `/maps`; on `maps.google.{domain}` it may
also be `/`.

| Link | Name | Point |
| --- | --- | --- |
| `/maps/place/{name}[/@{lat},{lon},{zoom}z][/data=…]` | `{name}` | The pin in `data` (`!3d{lat}!4d{lon}`), else the map's centre (`@`) |
| `/maps/search/{text}[/@{lat},{lon},{zoom}z]` | `{text}` | The text's coordinates, else the map's centre |
| `/maps/search/?api=1&query={text}` | `{text}` | The text's coordinates (`query_place_id` is not read) |
| `/maps/@{lat},{lon},{zoom}z` | None | The map's centre |
| `/maps?q={text}`, `maps.google.com/?q={text}` | `{text}` | The text's coordinates, else `ll` |
| `/maps?ll={lat},{lon}[&z={zoom}]` | None | `ll` |

A name in the path is read with `+` as a space and percent-decoded. A text is a name, or a point
written `{lat},{lon}`, `{lat},{lon}({name})` or `loc:{lat},{lon}`. A name that is only coordinates
(a dropped pin's `48.85, 2.29` or `48°51'30.2"N 2°17'40.2"E`) names no place: the card is the
point's.

Every spelling is its own resource: the source has no alias rule.

Not read here, so these links keep their route (the web route for `http` and `https` links):

- a CID or a place ID alone, such as `https://www.google.com/maps?cid=10222232094831998944`,
  `?ftid=0x…:0x…` or `?q=place_id:…`, since only Google's page could name it. The
  `wikidata_places` lookup still reads its CID;
- directions (`/maps/dir/…`, `saddr`, `daddr`);
- links with a port, a user name or a password, and Google pages outside `/maps`.

### Short links

A short link is `https://maps.app.goo.gl/{code}` or `https://goo.gl/maps/{code}`, with a code of 1
to 64 letters, digits, `_` and `-`. Its query (a share's `?g_st=…`) is not sent.

## What it fetches

A place link makes no request.

A short link makes one: a `GET` of the short link that does not follow the redirect. Pinakes reads
the answer's status and its `Location` header. It never reads the body: an answer with a body is
refused.

- A redirect (301, 302, 303, 307, 308) to a Google Maps link this source reads: the card is the
  target's, read from the target link. The short link becomes an [alias](../api.md#aliases) of
  the target (basis `source`), so the target's card answers for it and the `wikidata_places`
  lookup reads the target's CID. A redirect through Google's consent page
  (`consent.google.com/…?continue={target}`) is read at its `continue` link.
- A redirect to anything else (directions, a list, a CID alone, another site), a redirect without
  a `Location`, and a 400, 404 or 410: `missing`.
- A 429 waits at least the time the answer names, or 5 minutes when it names none. A 408 or a
  server error waits at least a minute.
- Any other answer (a page instead of a redirect, a 403) is an error, and the read is retried.

## Card

The type is `Place`, and `site` is "Google Maps".

- `name` is the place's name from the link, with runs of whitespace and control, format and
  bidirectional characters as one space, trimmed and cut to 256 bytes. A link without a name is
  named by its point, as a [`geo:` point](../api.md#points) is: the coordinates as written, or,
  from a map's zoom, rounded to its precision.
- `location` is the point from the link, with `location_source` `uri`. A link with a name and no
  point has no `location`.

| Fact | Meaning |
| --- | --- |
| `precision` | How precise the point is, from the map's zoom or the coordinates' decimal digits: `~10 km`, `~1 km`, `~100 m`, `~10 m` or `~1 m` |

## Keys and groups

The source records no keys. A place link's `data` names the place's feature ID
(`!1s0x{hex}:0x{hex}`); its second number is the place's CID, which Wikidata holds in decimal as
P3749. The `wikidata_places` lookup ([places](places.md)) finds the one item with that CID and adds
its key and links. Names and points join nothing.

## Credit

The card credits nothing.

## Re-extraction

A Google Maps link stored on the web route gets this source's card after `pinakesd reextract web`.

## Turn it off

```toml
[sources.google_maps]
enabled = false
```

Google Maps links then get ordinary web cards.

## Examples

- `https://www.google.com/maps/place/Eiffel+Tower/@48.8583701,2.2919064,17z/data=!3m1!4b1!4m6!3m5!1s0x47e66e2964e34e2d:0x8ddca9ee380ef7e0!8m2!3d48.8583701!4d2.2944813!16zL20vMDJqODE`
- `https://www.google.com/maps/search/?api=1&query=47.5951518,-122.3316393` (a point)
- `https://maps.app.goo.gl/9ivZQ71CKMqRPDtQA` (a short link)
