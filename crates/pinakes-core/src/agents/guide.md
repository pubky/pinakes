# Searching the links people share on Pubky (Pinakes)

Pinakes is a public, read-only index of the external links people reference on Pubky: web pages,
places, events, books, music, videos, papers, posts on other networks and objects of other Pubky
apps. Each link is read into a small typed card. You ask by text, type, tag, map area, time or an
exact identifier, and get cards back as JSON. No account, no API key, no install.

**Base URL: the host that served you this document**, `{{BASE}}` here. Every instance indexes the
links its own Nexus reported, so another instance answers a different question.

## Quickstart

One GET is most of the API:

```sh
curl -s '{{BASE}}/v1/search?q=lugano&limit=5'
```

```json
{"items":[{"id":"…","uri":"https://…","status":"ok","type":"Place","name":"…","description":"…","point":{"lat":46.0037,"lon":8.9511},"start":null,"site":"OpenStreetMap","group":null,"also_on":[]}],"cursor":"…","reach_truncated":false}
```

That is the shape only. `items` are cards; `cursor` is the next page (pass it back as `cursor`
with the same filters), `null` at the end.

To read one link you already have:

```sh
curl -s '{{BASE}}/v1/resources/by-uri?uri=https://www.openstreetmap.org/node/240109189'
```

A link this instance does not know is a 404 there: nobody referenced it, or it has not been read.

## What is in the index

- **Cards.** `type` is a schema.org type (`Place`, `Event`, `MusicEvent`, `Book`,
  `ScholarlyArticle`, `MusicRecording`, `VideoObject`, `SocialMediaPosting`, `WebPage`, …).
  `name`, `description`, `image`, `site`, `point` (`lat`, `lon`), `start` and `end` are what the
  link's page or the service's own API says. `status` is `ok`, `pending`, `missing` (the object
  is gone upstream), `unreachable` or `error`.
- **Only referenced links.** Search lists links someone tagged, replied to or embedded on Pubky.
  Pinakes holds no posts, users or tags of its own: those are Nexus's (below).
- **Groups.** Links that are the same thing on several services (a song on Spotify and on
  Apple Music, a book's editions, an event on two platforms) form a group: `group.id` on each
  card, `also_on` lists the others. Every link keeps its own card.
- **Places and events.** `point` places a card on the map; events have `start`, `end` and, for
  calendars, one card per occurrence.
- **Identifiers.** Books, papers, music, films, podcasts and OSM objects carry exact keys:
  `isbn:`, `doi:`, `isrc:`, `upc:`, `mbid:`, `wikidata:`, `imdb:`, `podcast:`, `osm:`.
- **Other Pubky apps.** `pubky://` objects of apps such as Mapky and Eventky are cards too, with
  `open_in` naming the clients that open them.

`GET {{BASE}}/v1/search?facet=type&limit=1` counts the card types this instance holds; each
entry's `type_path` (`["Thing", "CreativeWork", "MusicRecording"]`) places it in the schema.org
tree. `type` takes several types, any of them: `type=MusicRecording,MusicAlbum,MusicGroup`.

## Recipes

**What is this link?**

```sh
curl -s '{{BASE}}/v1/resources/by-uri?uri=https://www.imdb.com/title/tt0111161/'
```

Any spelling works (`http`, `www.`, a trailing slash, tracking parameters, a short link the
index knows as an alias). A read is one flat object: the card's fields, and beside them the
address, facts from official APIs, every `also_on` link, the keys `key=` takes (`keys`) and the
other spellings (`aliases`). To have an unknown link read, see
`POST /v1/resolve` under Limits.

**Places near a point, tagged something.**

```sh
curl -s '{{BASE}}/v1/search?type=Place&near=47.3769,8.5417&radius=2000&tags=coffee&sort=distance'
```

`near` is `lat,lon`, `radius` metres (at most 200 km by default). `tags` takes up to five
comma-separated labels, any of them; add `match=all` for all of them (at most three). For a map
viewport use `bbox=minLon,minLat,maxLon,maxLat` instead of `near`.

**Events in Zürich this week.**

```sh
curl -s '{{BASE}}/v1/search?type=Event&near=47.3769,8.5417&radius=15000&from=now&to=now%2B7d&sort=start'
```

`from` and `to` take RFC 3339, Unix milliseconds or `now`, `now+7d`, `now-12h`. In a query
string write `+` as `%2B` (a bare `+` is read as a space, which Pinakes also accepts as `+`).
`type=Event` includes `MusicEvent` and every other event type, and calendar dates.

**Remote jobs posted this month.**

```sh
curl -s '{{BASE}}/v1/search?type=JobPosting&remote=true&from=now-30d'
```

A job posting's `start` is the date it was posted and its `end` its closing date; `from` and
`to` select by the date posted. `remote=false` keeps the postings that may not be done
remotely, with or without a workplace; `near` or `bbox` find those with one on the map. A closed
posting is left out unless you add `closed=true`.
`facts` holds the employer, employment type, salary, remote regions, apply link and
`job_status` (`open` or `closed`).

**Everything with one identifier.**

```sh
curl -s '{{BASE}}/v1/search?key=isbn:9780140328721'
curl -s '{{BASE}}/v1/search?key=doi:10.1038/nphys1170'
curl -s '{{BASE}}/v1/search?key=isrc:GBAYE9701376'
curl -s '{{BASE}}/v1/search?key=osm:node/240109189'
```

One key per search, in any spelling (ISBN-10 or ISBN-13, an ISRC with hyphens). An unknown
namespace is a 400.

**What else is the same thing?** Read the card and follow its group:

```sh
curl -s '{{BASE}}/v1/resources/by-uri?uri=https://open.spotify.com/track/…'
curl -s '{{BASE}}/v1/groups/{group.id}'
```

The group lists every member's card. `also_on` on a card already names one link per service,
including links nobody tagged that a source derived (`derived: true`).

**One row per thing, not per link.** Add `collapse=group` to a search: one row per group, the
member with the most references standing for it.

**What is popular.** `sort=trending&window=7d` ranks by references made in the window;
`sort=recent` by the newest reference; the default `activity` by taggers and references with a
30-day decay.

## Joining with Nexus by URI

Pinakes ranks, Nexus hydrates. Pinakes's `id` is its own: join the two indexes by **URI**, never
by id. Who tagged a link, with which labels, and what your own follows think of it are Nexus's.

1. Search Pinakes: take each card's `uri`.
2. Send those URIs, 100 at a time, to the Nexus batch resource view:

```sh
curl -s "$NEXUS/v0/stream/resources/by_ids" -H 'content-type: application/json' \
  -d '{"uris":["https://www.openstreetmap.org/node/240109189"],"viewer_id":"<your pubky key>"}'
```

Each answer has `details.uri`, `tags` (`label`, `taggers_count`, and with `viewer_id`
`relationship`: whether the viewer used that label) and `taggers_count`, in request order;
unknown URIs are skipped. Through MCP, the `social` tool makes this call to the Nexus this
instance reads.

For graph questions (who follows whom, trust, threads, a user's tags) use **nexus-scout**
(`https://nexus-scout.pubky.app/llms.txt`): read-only Cypher over the Pubky social graph. To keep
only links your network referenced, search here with `reach=following&viewer=<pubky key>` (or
`friends`); to rank them first and keep the rest, use `boost` instead of `reach`.

## MCP

`POST {{BASE}}/mcp` is a Model Context Protocol endpoint (Streamable HTTP, stateless, read-only,
no auth). Its tools are thin calls into the routes above, and each answer names the REST URL with
the full result:

| tool | does |
| --- | --- |
| `search` | `GET /v1/search`: `q`, `type` (one or several), `tags`, `match`, `near` + `radius`, `bbox`, `from`, `to`, `key`, `remote`, `closed`, `sort`, `collapse`, `reach` or `boost` + `viewer`, `limit`, `cursor` |
| `card` | One link's card by URI (`GET /v1/resources/by-uri`); `resolve: true` asks `POST /v1/resolve` for an unknown link |
| `lookup` | Every card with one identifier (`isbn:…`, `doi:…`, `isrc:…`, `osm:…`, …) |
| `group` | A group's members by group id |
| `types` | The card types this instance holds, with counts, and the kinds of links it resolves |
| `social` | 1–100 URIs through the Nexus this instance reads: tags and tagger counts per URI |

Add it to a client as a remote (HTTP) MCP server at `{{BASE}}/mcp`.

## Limits

- **Per-IP budget**: 120 requests a minute over every public route by default
  (`limits.requests_per_minute` in `GET {{BASE}}/v1/info`). A `reach` search costs 6, a `related`
  search 6. Over budget is a 429 with `Retry-After: 60`. An MCP message costs one, and a tool
  call costs what its REST call costs on top.
- **Pages**: `limit` 1–500, default 50. Follow `cursor`; a full page does not mean there is no
  more.
- **Inputs**: a URI at most 2048 bytes, `q` at most 256 bytes, at most 5 tags, bodies at most
  2 MB.
- **Reading new links**: `POST {{BASE}}/v1/resolve` with `{"uri": "…"}` reads a link now (within 3
  seconds, else 202 `pending`: read it again later by id). It has its own smaller budget
  (`limits.previews_per_minute` and `previews_per_uri_per_minute` in `/v1/info`). Search never
  fetches, and nothing you resolve is listed by search until someone references it on Pubky.

## Errors

Errors are `{"error": "…"}` with the status:

| status | meaning | what to do |
| --- | --- | --- |
| 400 | A parameter Pinakes cannot use; the message names it | Fix it. A 400 on a `cursor` means the daemon restarted or the filters changed: search again without it |
| 404 | An unknown link, resource or group | For a link: nobody referenced it here. Resolve it, or search by `q` or `key` |
| 413 | Body over 2 MB | Send less |
| 429 | Over the per-IP budget | Wait the `Retry-After` seconds. Ask fewer, larger questions: one search with `facet`, or `POST /v1/resources/lookup` for up to 500 links at once |
| 502 | Nexus did not answer a `reach` search in time | Retry once, or drop `reach` |
| 500 | Internal | Retry once |

`GET {{BASE}}/v1/openapi.json` describes every route and parameter.
