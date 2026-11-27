# Apple Maps

Pinakes reads a place's name and point from an Apple Maps link itself. It never asks Apple's pages
or APIs, and makes no request. No key or account is needed.

| | |
| --- | --- |
| Source | `apple_maps`, official source |
| Links | `maps.apple.com/?q=…&ll=…`, `/?address=…&ll=…`, `/place?coordinate=…&name=…` |
| Fetches | Nothing |
| Credentials | None |
| Rate | None (no request) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)); a refresh makes no request |
| Card types | `Place` |
| Keys | None |
| Credit | None |
| Switch | `[sources.apple_maps] enabled`, on by default |

## Links

The source reads links on `maps.apple.com` at `/` or `/place` that name a place or a point:

- the name is `name`, else `q`, else `address`: the first of them that names something;
- the point is `coordinate`, else `ll` (`{lat},{lon}`), else a `q` written as coordinates.

A name that is only coordinates (a dropped pin's) names no place: the card is the point's. Apple's
own IDs (`auid`, `place-id`) are not read, and nor are `sll` and `near` (where a search looks).

Every spelling is its own resource: the source has no alias rule.

Not read here, so these links keep their route (the web route):

- a link with an Apple ID alone, such as `https://maps.apple.com/place?auid=1218585579701879383`,
  since only Apple could name it. The `wikidata_places` lookup ([places](places.md)) still reads
  its `auid`;
- directions (`saddr`, `daddr`, `dirflg`, `/directions`), searches at `/search`, and other paths;
- links with a port, a user name or a password.

## What it fetches

Nothing.

## Card

The type is `Place`, and `site` is "Apple Maps".

- `name` is the place's name from the link, decoded, with runs of whitespace and control, format
  and bidirectional characters as one space, trimmed and cut to 256 bytes. A link without a name is
  named by its point, as a [`geo:` point](../api.md#points) is: the coordinates as written.
- `location` is the point from the link, with `location_source` `uri`. A link with a name and no
  point has no `location`.

| Fact | Meaning |
| --- | --- |
| `precision` | How precise the point is, from the coordinates' decimal digits: `~10 km`, `~1 km`, `~100 m`, `~10 m` or `~1 m` |

## Keys and groups

The source records no keys. Names and points join nothing. A link with an `auid` on `/place` gets
the `wikidata_places` lookup ([places](places.md)).

## Credit

The card credits nothing.

## Re-extraction

A Apple Maps link stored on the web route gets this source's card after `pinakesd reextract web`.

## Turn it off

```toml
[sources.apple_maps]
enabled = false
```

Apple Maps links then get ordinary web cards.

## Examples

- `https://maps.apple.com/place?address=Champ%20de%20Mars,%205%20Avenue%20Anatole%20France,%2075007%20Paris,%20France&coordinate=48.858260,2.294500&name=Eiffel%20Tower`
- `https://maps.apple.com/?q=Eiffel+Tower&ll=48.858260,2.294500`
- `https://maps.apple.com/?ll=50.894967,4.341626` (a point)
