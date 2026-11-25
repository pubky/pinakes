# USGS earthquakes

Pinakes reads earthquakes and other seismic events of the U.S. Geological Survey's Earthquake
Hazards Program from its FDSN event service, by event ID. No key is needed.

| | |
| --- | --- |
| Source | `usgs`, official source |
| Links | `earthquake.usgs.gov/earthquakes/eventpage/{id}` (any tab), `earthquake.usgs.gov/earthquakes/feed/v1.0/detail/{id}.geojson`, `earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}`; root `usgs:{id}` |
| Fetches | `GET https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}&format=geojson` |
| Credentials | None |
| Rate | 1 request per second on `earthquake.usgs.gov` |
| Refresh | Daily, as every official card; a past event becomes final ([refresh](../design.md#refresh)) |
| Card types | `Event` |
| Keys | None (facts only) |
| Credit | "U.S. Geological Survey", in `attribution` |
| Switch | `[sources.usgs] enabled`, on by default |

## Links

An event ID is a network code followed by that network's own code, in lower case, such as
`ci38457511`, `us7000abcd` or `official20110311054624120_30`. It holds lower-case letters, digits,
`_` and `-`, up to 64 characters.

Every USGS address of one event is one resource. The root is `usgs:{id}`, and these links are
[aliases](../api.md#aliases) of it:

- `https://earthquake.usgs.gov/earthquakes/eventpage/{id}`, with any tab (`/executive`, `/map`,
  `/origin/detail`, …), query or fragment;
- `https://earthquake.usgs.gov/earthquakes/feed/v1.0/detail/{id}.geojson`;
- `https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}`, with any other parameters but
  exactly one `eventid`.

A link's ID is case-insensitive. A root must hold the ID in lower case: a `usgs:` root that is not
a valid lower-case event ID, such as `usgs:CI38457511`, is `missing` at once and never fetched.

### Preferred IDs

One event usually has several IDs, one from each network that reported it, and one of them is the
preferred ID. The card's root is `usgs:{preferred id}`. A link with another of the event's IDs has
`usgs:{that id}` as its root until the first fetch, which names the preferred ID. From then on the
link and that root are aliases of the preferred root. The `ids` fact lists every ID of the event.

### Not read here

The map and the summary feeds (such as `feed/v1.0/summary/all_day.geojson`), detail feeds in other
formats (such as `.json`), FDSN queries without exactly one `eventid`, and other seismology
services.

## What it fetches

One request per read:

`GET https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}&format=geojson`

The answer is the event with all its products, and can be over 100 KB. An ID the service doesn't
know (404), an event it deleted (409) and an empty answer (204) are `missing`. An answer for an
event that doesn't have the asked ID among its IDs is an error, and the read is retried.

### Slow answers

The service can take 10 to 25 seconds to answer for an event it hasn't served recently. That is
longer than a request may take. The service keeps an answer only once it has delivered it, and
for under a minute, so an immediate retry is not sure to be faster.

A read that times out is tried again after 10 minutes, and these retries don't count as failures.
After three days of slow answers the link is `unreachable` and is retried with the usual growing
backoff ([refresh](../design.md#refresh)). It is never given up.

## Card

The type is `Event`.

- `site` is "USGS" and `name` the title, for example "M 9.1 - 2011 Great Tohoku Earthquake,
  Japan". The card has no description and no image.
- `start` is the origin time, and the location is the epicenter.
- The `page_url` fact is the event page,
  `https://earthquake.usgs.gov/earthquakes/eventpage/{preferred id}`. Because the root
  `usgs:{id}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "U.S. Geological Survey".

| Fact | Meaning |
| --- | --- |
| `magnitude` | The preferred magnitude |
| `magnitude_type` | For example `mww`, `mw`, `ml` |
| `depth_km` | The depth of the hypocenter, in kilometers |
| `place` | USGS's description of the place |
| `event_type` | `earthquake`, `quarry blast`, `explosion`, … |
| `tsunami` | `true` when USGS flags the event for tsunami information |
| `alert` | The PAGER alert level: `green`, `yellow`, `orange` or `red` |
| `status` | `reviewed` or `automatic` |
| `felt` | The number of "Did You Feel It?" reports |
| `significance` | USGS's significance score |
| `ids` | Every ID of the event |
| `page_url` | The event page on earthquake.usgs.gov |

## Keys and groups

None. Links by the event's other IDs become aliases of its root; the IDs are not keys, and the card joins no
[group](../api.md#groups).

## Credit

The `attribution` line credits the U.S. Geological Survey.

## Turn it off

```toml
[sources.usgs]
enabled = false
```

USGS links then get ordinary web cards, and `usgs:` roots wait until the source is on again.

## Examples

- `https://earthquake.usgs.gov/earthquakes/eventpage/official20110311054624120_30` (the 2011
  Tohoku earthquake)
- `https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/executive` (the 2019 Ridgecrest
  earthquake)
