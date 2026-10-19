# USGS earthquakes

Earthquakes and other seismic events from the U.S. Geological Survey's Earthquake Hazards
Program, read from its FDSN event service by event id. Public domain; no key; no published rate
limit (one request per second). Cards carry the attribution "U.S. Geological Survey".

## Links

An event id is a network code and that network's own code, in lower case (`ci38457511`,
`us7000abcd`, `official20110311054624120_30`). `usgs:{id}` and every USGS address of an event are
one resource:

- `https://earthquake.usgs.gov/earthquakes/eventpage/{id}`, with any tab (`/executive`, `/map`,
  `/origin/detail`, …), query or fragment;
- `https://earthquake.usgs.gov/earthquakes/feed/v1.0/detail/{id}.geojson`;
- `https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}` (any other parameters).

One event usually has several ids, one from each network that reported it, and one of them is
the preferred id. The root is `usgs:{preferred id}`: a link with another of the event's ids has
`usgs:{that id}` as its root until the first fetch, which names the preferred id; from then on
both are aliases of the preferred root. The event's ids are listed in the `ids` fact.

An id the service does not know (404), or an event it has deleted (409), is `missing`.

## What it reads

`GET https://earthquake.usgs.gov/fdsnws/event/1/query?eventid={id}&format=geojson` (the event with
all its products; often over 100 KB).

## Card

`Event`, with `site` "USGS", `name` the title (e.g. "M 9.1 - 2011 Great Tohoku Earthquake,
Japan"), `start` the origin time, and the epicentre as the location.

| Fact | Meaning |
| --- | --- |
| `magnitude` | The preferred magnitude |
| `magnitude_type` | E.g. `mww`, `mw`, `ml` |
| `depth_km` | Depth of the hypocentre in kilometres |
| `place` | USGS's description of the place |
| `event_type` | `earthquake`, `quarry blast`, `explosion`, … |
| `tsunami` | `true` when USGS flags the event for tsunami information |
| `alert` | PAGER alert level: `green`, `yellow`, `orange` or `red` |
| `status` | `reviewed` or `automatic` |
| `felt` | Number of "Did You Feel It?" reports |
| `significance` | USGS's significance score (0 and up) |
| `ids` | Every id of the event |
| `page_url` | The event page on earthquake.usgs.gov |

## Examples

- `https://earthquake.usgs.gov/earthquakes/eventpage/official20110311054624120_30` (the 2011
  Tohoku earthquake)
- `https://earthquake.usgs.gov/earthquakes/eventpage/ci38457511/executive` (the 2019 Ridgecrest
  earthquake)
