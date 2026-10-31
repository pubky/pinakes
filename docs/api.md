# Pinakes API v1

Pinakes returns link metadata (cards), search and ranking over the links people reference on
Pubky, calendar dates, groups of links to the same thing, and the changes to the links a client
keeps. Clients hydrate social state (posts, tags, users) from Nexus in a separate call, by URI:
a card's `uri` (and `aliases`), never its `id`, which is Pinakes's own. Paths are
relative to the Pinakes origin.

The public listener (`listen`) serves its OpenAPI document at **`GET /v1/openapi.json`**
([openapi.json](openapi.json)), the routes below. Swagger UI at **`/swagger-ui`**
(`api.swagger_ui`, default on) renders it from a vendored bundle, so "try it out" only reaches
public operations. The admin listener's routes are described under
[Admin listener](#admin-listener); it serves no document. `scripts/openapi` regenerates the
snapshot; a test fails when it is stale.

## Routes

| Method and path | Input | Response |
| --- | --- | --- |
| `GET /v1/search` | Query parameters ([Search](#search)) | `{items: [card], cursor, reach_truncated, facets?}`, or a map page with `cluster` |
| `GET /v1/resources/{id}` | 32-character resource ID | The card with the [read's fields](#resource-read); 404 if unknown |
| `GET /v1/resources/by-uri` | `uri`: any spelling of a link | As `GET /v1/resources/{id}` for the URI's ID (an alias answers with its root); 404 if unknown, nothing is fetched |
| `POST /v1/resources/lookup` | Exactly one of `{"ids":[…]}` (alias `resource_ids`) or `{"uris":[…]}`, 1–500 entries | `[card]` in request order, duplicates kept, unknowns skipped |
| `POST /v1/resolve` | `{"uri":"https://example.org/"}` | The card with the read's fields now or within 3 seconds; 202 with `status: "pending"` while queued, with `Retry-After` (seconds) when the next attempt is put off |
| `GET /v1/resources/{id}/occurrences` | Optional RFC 3339 `from`, `to`, `limit` (1–500, default 100), signed `cursor` | `{items: [occurrence card], cursor}` |
| `POST /v1/occurrences/lookup` | `{resource_ids?, uris?, from?, to?, limit?, cursor?}`: 1–500 feed IDs and links together | `{items: [occurrence card], cursor}` across the feeds, by start |
| `GET /v1/groups/{id}` | 32-character group ID | `{id, type, curated, keys, members: [card]}`; 404 if unknown |
| `POST /v1/changes/lookup` | `{resource_ids?, uris?, since?, cursor?, limit?}`: 1–500 IDs and links together | Change page for those resources |
| `GET /v1/info` | None | `{version, api_shape, user_agent, resolves, limits, osm_licence}`: the shape of its cards (`api_shape: 2`, one flat object per card), what this instance resolves, the limits a client must respect, and the User-Agent its requests carry |
| `GET /v1/dumps/osm_place.ndjson.gz` | None | The ODbL derivative database as a daily gzip file ([OSM](#openstreetmap-and-locations)) |
| `GET /v1/openapi.json` | None | OpenAPI 3.1 |
| `GET /swagger-ui` | None | Swagger UI (when `api.swagger_ui`) |
| `GET /` | None | The index for agents: the guide, the skill, the MCP endpoint, this document and nexus-scout ([Agents](#agents)) |
| `GET /llms.txt` | None | The usage guide for agents, plain text ([Agents](#agents)) |
| `GET /skill.md` | None | The guide as an agent skill named `pinakes` ([Agents](#agents)) |
| `POST /mcp` | One MCP JSON-RPC message | The MCP endpoint (when `api.mcp`; [Agents](#agents)); not in the OpenAPI document |
| `GET /mcp` | None | A browser (`Accept: text/html`): a short page on how to connect; any other GET: 405 with `Allow: POST` ([Agents](#agents)) |

URIs are normalized and IDs computed by Pinakes itself: lowercase scheme and host, no default
port, userinfo or fragment, then BLAKE3. Each URI is at most 2048 bytes; IDs are 32 lowercase hex
characters and Pinakes's own, so combine with other indexes by URI. A missing upstream object is a known resource with
`status: "missing"`, not a 404. `lookup` never fetches an unknown URI; a known overdue resource
moves to the front of the refresh queue without bypassing its TTL, backoff or final state.

## Response shapes

Every route returns the same card for a resource, an occurrence or a Pubky event reference.
Absent values are `null`, never `""`. Text fields (`name`, `description`, `site`, and every
text a read adds) are plain text, decoded from the source's HTML entities once: escape them
before rendering them as HTML.

A music link is typed by what it is, whatever the service's page or player calls it: a track is a
`MusicRecording`, an album a `MusicAlbum`, an artist a `MusicGroup` (a `Person` from the next
refresh after MusicBrainz says the artist is one) and a playlist a `MusicPlaylist`, on every
service ([music service links](#aliases)). A video stays a `VideoObject`; a track's, album's or
artist's page keeps its own video (a `VideoObject` in its JSON-LD) as a part of the page, not its
card ([lists](#lists)). The card's `site` is the
service's name (`Spotify`, `Apple Music`, `TIDAL`, `YouTube`, …), as derived links show it, in
any page language. A Bandcamp root or `/music` page features an album but is the artist's page: a
`MusicGroup` named after the artist, without the album's description. When the service's own
page names the artist (JSON-LD `byArtist`, as TIDAL's do), the description says what the page
does instead of its share text: a track's `Queen · A Night At The Opera (Deluxe Edition) · 5:55`
(artist, album, length), an album's `Tame Impala & JENNIE · 2025` (artist, year); the
`service` source adds `artist`, `album` and `duration` (seconds) to `facts`.

A media link is typed the same way: a TMDB, Letterboxd or Trakt movie is a `Movie`; a TMDB,
Trakt or TVmaze series a `TVSeries`; an Open Library work or edition and a Goodreads work or book
a `Book`; an Apple Podcasts, Podcast Index, Pocket Casts, Castro or Fountain show a
`PodcastSeries` and its episode a `PodcastEpisode`. A Netflix title is a `Movie` or `TVSeries`
when its Wikidata item says which, else a `CreativeWork`. A Wikidata item that is a creative
work (a series, a film, a book, an album, a song, a game, a podcast or a periodical by what it is
an instance of, P31; else one with a publication date, an author, a director, a composer, a
publisher or an episode count) is that work (`TVSeries`, `Movie`, `Book`, …), never an `Event`,
whatever dates it carries: a series' run is not an occurrence. A page that is a podcast episode
by its feed ([also on](#also-on)) is a `PodcastEpisode` from its next refresh.

Such a card is named after its object, not its page: where a TMDB, Trakt, Netflix, Goodreads,
Letterboxd, Open Library, TVmaze or Podcast Index page names its site, another language's title
or nothing, the name (and, where the page's text is not the object's, the description) comes
from the object's exact record: the Wikidata item that carries the page's own ID, or the
service's own record (`media_records`, [docs/sources](sources/README.md)). That record is read while the
page is, within a few seconds; when Wikidata is slower, the card keeps the page's name at first.
The `titles` source then reads the same item without that limit and, when the card bears another
name, has the link read again: its card then takes the item's label (and description where the
page's is not the object's), usually within minutes.
An IMDb title keeps the type its page gives (it can be either).

### Card

| Field | Meaning |
| --- | --- |
| `id` | Resource ID; for an occurrence, the feed's ID |
| `uri` | The resource URI; for an occurrence, its own URL when it has one, else the feed's. To combine with Nexus, send `uri` and `refs`, and for an occurrence `occurrence.feed_uri` (its own URL was never tagged) |
| `refs` | The links people referenced on Pubky that resolve to this card, most referenced first (counted up to 1,000 each, then by URI), at most 5: its own `uri` and its aliases that carry references (the 31 most recently referenced), normalized as every URI is. A typed card's `uri` is often a link nobody wrote (`btc:block/…`, `uniprot:…`, `isbn:…`) while people tagged the mempool.space, UniProt or Open Library page: ask Nexus about each of `refs` too and add up the card's tags over them. An occurrence's are its feed's; a gone social post's, its own link alone. `[]` when nothing references the card |
| `status` | Fetch health: `ok`, `pending`, `missing`, `unreachable` or `error` |
| `type`, `type_path` | schema.org type (`MusicEvent`, `Book`, `Place`, …), or one of Pinakes's own types for what schema.org has no class for: `BitcoinBlock`, `BitcoinTransaction`, `BitcoinAddress`, `LightningNode` and `LightningChannel` (not schema.org; each a child of `Thing`); `type_path` is the type's ancestors, root first (`["Thing", "Event", "MusicEvent"]`), the types a `type=` search matches it by |
| `name`, `description`, `image` | Display metadata; images point to the origin and are never proxied |
| `stream` | Audio a client can play, `{url, type, duration_s, preview}` ([Stream](#stream)): a podcast episode's enclosure, a recording's public preview. Absent when the card has none |
| `site` | The publishing site: the page's own site name (`og:site_name`), else its host; for `pubky://` objects the app path segment (`loopky`), not the public key; for a link whose URL names a known service (a music service, a Wikipedia edition, MusicBrainz, Wikidata, a service the also-on lookups know), that service's name (`Apple Music`, `Wikipedia (de)`, `OpenStreetMap`), whatever the page calls itself and even before its card is read; for a card read from an official API without a host of its own, the service's name (`Federal Register`) |
| `point` | `{lat, lon}` |
| `start`, `end` | RFC 3339 instants for events and occurrences |
| `group` | `{id, size}` when the resource or occurrence is in a [group](#groups), else `null` |
| `also_on` | The same thing elsewhere ([Also on](#also-on)): the other members of its group as `{id, uri, site}` (other resources first, one entry each by ID, then other occurrences of its own feed), then derived links nobody tagged as `{uri, site, derived: true, via, kind?, at?}`, by service name; one per service, at most 8 on a search, lookup or group card; on a resource read every link, several of one service too, up to 256. A hint only: every member keeps its own card |
| `open_in` | The clients that open a Pubky app's object, `[{client, url}]` (`url` is `https:`): from the app's profile ([Pubky apps](pubky-apps.md)), e.g. Loopky for a deck; on a Pubky event reference's card, those of the event object. `[]` on every other card |
| `attribution` | Present on OSM cards, events placed at an OSM venue and events whose zone came from coordinates; display it with the card |
| `occurrence` | Occurrence cards only: `{feed_id, feed_uri, uid, recurrence_id, status}`; `feed_uri` is the feed's URI, the link people tagged |
| `event` | Pubky event references only (an event post's occurrence at a place; no feed writes them today: social events are Nexus's, see [Pubky events](#calendars-and-occurrences)): `{author, object_path, recurrence_id, start, end, status}`; `object_path` is the post's versionless path, `recurrence_id` the occurrence's local start (`YYYY-MM-DDTHH:MM:SS`) or null for a one-off |
| `maybe_same` | Event and occurrence cards: other events that are [possibly the same event](#possibly-the-same-event), best first, each `{id, uri, site, start, score}` (`[]` when there are none). An app's store card: store cards that are [possibly the same app](#possibly-the-same-app). `MusicRecording` cards: other recordings that are [possibly the same recording](#possibly-the-same-recording), the same shape (`start` is `null`). Up to three on a search, lookup or group card, up to ten on a resource read. A hint only, like `group`. Absent on other cards, on event and app cards with `[event] same_event_hint = "off"` and on recording cards with `[groups] same_recording_hint = "off"` |

A `pubky://` object of an app Pinakes has a profile for (Loopky, Pubky Rooms,
the Pubky marketplace) gets that app's card; any other app's object gets a general one. Each is
in [Pubky apps](pubky-apps.md). An object without a name, title or description takes its
description from a content-like member (`content`, `text`): such a card is a
`SocialMediaPosting`, social content like a note, with a 200-character excerpt that refreshes
within the hour and is removed once the object is gone.

A social post found gone (a note, or a post or comment an official-API source reads) keeps
nothing of itself. Its card is `status: "missing"` with its `uri`, `site` and `from`, and the
type every gone post has, `SocialMediaPosting`, whatever the post was (a Reddit thread, a TikTok
video): no name, description, image or facts, no also-on links and no change history. Its read
lists no `aliases`, and its `refs` name its own link alone: a spelling's link can carry the post's
title.

### Stream

One field for everything playable, so a client needs one player for episodes and recordings:

```json
"stream": {"url": "https://…/ep180.mp3", "type": "audio/mpeg", "duration_s": 3557, "preview": false}
```

| Field | Meaning |
| --- | --- |
| `url` | The audio itself, for an `<audio>` element: never a player page. Pinakes never fetches or proxies it |
| `type` | Its MIME type without parameters, always `audio/…` or `video/…`, at most 127 bytes |
| `duration_s` | Its length in whole seconds when its source states it, else `null`. A preview's is usually `null`; the recording's own length is the fact `duration` |
| `preview` | `true` for a short sample of a longer recording (a music service's 30-second preview), `false` for the whole of it (an episode) |

Only public, direct links are a stream:
- `https` or `http` on a named host: no IP address, `localhost` or single-label name, and no
  user or password;
- no query parameter that signs the link or makes it expire (`sig`, `hdnea`, `hmac`, `auth`,
  `expires`, `exp`, `policy`, `Key-Pair-Id`, any `X-Amz-…` or `X-Goog-…`, any name with `token`
  or `signature` in it), so a link that stops working, or works only for whom it was made, is
  never one;
- at most 2,048 bytes, and at most a week long.

A feed or page behind a login is never read, so nothing behind one is a stream. A publisher's
tracking redirect (`podtrac.com`, `pdst.fm`) is its public link and stays. No DRM service's audio
is one: Spotify's, Apple Music's and TIDAL's own players are not streams.

Which cards carry one (each source's page in [docs/sources](sources/README.md) says how):
- a podcast feed's episode (`{feed}?guid={item}`, or the show's own episode page whose feed item
  links back to it): the item's first audio or video `<enclosure>`, with its `itunes:duration`;
- a Podcast Index episode: the enclosure Podcast Index records (the `podcast_index` source);
- a Castro episode: the file its page plays (the `castro` source);
- any other episode the also-on `podcasts` source reads (an Apple Podcasts
  or Fountain episode, a page its feed's item links back to): the enclosure it hears of, from
  the card's next read (the source asks for one at once);
- an Apple Music track: iTunes' 30-second preview, `preview: true` (the `itunes` source), from
  the card's next read. Deezer's previews are signed and expire, so they are never one;
  Spotify's are not used.

### Resource read

`GET /v1/resources/{id}`, `GET /v1/resources/by-uri` and `POST /v1/resolve` return the card with
these fields beside the card's: one flat object (`api_shape: 2` in `/v1/info`; there is no
`details` object). The read's `also_on` lists every link, up to 256, and its `maybe_same` up to
ten. The two reads carry `Cache-Control: public, max-age=30` and an `ETag` of the body, and
answer `If-None-Match` with 304.

| Field | Meaning |
| --- | --- |
| `category` | OSM `key/value` category or the resolver's category |
| `address` | `{street, house_number, postcode, city, country_code}` parts present |
| `bbox` | `[min_lon, min_lat, max_lon, max_lat]` |
| `tz` | IANA zone of an event |
| `all_day` | An all-day event (its `end` is exclusive) |
| `event_status` | `scheduled`, `cancelled`, `postponed`, `rescheduled` or `moved_online` |
| `attendance` | `offline`, `online` or `mixed` |
| `venue` | `{name, id}`: the venue's name and, when it resolved to a resource, its ID |
| `organizer` | `{name, url}` |
| `from` | Where the card's metadata came from when it is not the page itself: the [source](#sources) whose official API answered, e.g. `federal_register` |
| `facts` | The fields that API names for the object, by name (`document_type`, `citation`, …), or the facts a [Pubky app's](pubky-apps.md) profile reads (`card_count`, `price`, `rating`, …), then the facts the [also-on](#also-on) sources read for it (such as a music link's `isrc`, `upc` and `mbid`); several values of one name are joined with `; `. Each source keeps at most `limits.also_on_facts_per_source` (32, at least 8) facts per resource, cutting the many values of one name (a recording's ISRCs) before a name's one value (its MBID, artist). Each source's page lists its facts. `null` when there are none. Facts never join anything |
| `keys` | The public identifiers the resource is keyed by, as `GET /v1/search?key=` takes them (`isrc:…`, `upc:…`, `doi:…`, `isbn:…`, `mb:recording:…`, `wikidata:Q…`, `osm:node/…`): the keys its sources vouch for ([Also on](#also-on)), sorted; `[]` when there are none |
| `aliases` | Other spellings of this link that resolve to it ([aliases](#aliases)), at most 50; a page's own `url`, `@id`, `sameAs` or canonical claims are not aliases and are not listed. The card's `uri` is the canonical one: an alias is answered with its root's card |
| `updated_at` | When the current metadata was accepted |

Nothing that describes the fetch rather than the thing is public (the resolver, TTL, group keys,
raw attributes, parser warnings, content type and length): operators read the stored document
with `GET /v1/resources/{id}/document` on the admin listener.

### Occurrence

An occurrence card is a card with `occurrence: {feed_id, feed_uri, uid, recurrence_id, status}`. A null
`recurrence_id` is a non-recurring instance; recurring keys are UTC instants or all-day dates.
`occurrence.status` is the instance's lifecycle (`scheduled`, `cancelled`, `postponed`,
`rescheduled`, `moved_online`); the card's own `status` is the feed's fetch health. Use
`(feed_id, uid, recurrence_id)` as the client's row key: `id` alone names the feed.

### Group

`GET /v1/groups/{id}` returns `{id, type, curated, keys, members}`: `type` is the type most
members have (ties by name), `keys` the identifiers two or more members share (`cite:…`,
`ics_feed:…`, `ics_uid:…`; none for a group an operator set), and `members` every member's own
card, by ID and then member (an occurrence is its own member).

### Change line

`{seq, resource_id, occurrence?, at, class, fields, root?}`: `at` is Unix milliseconds,
`occurrence` the serialized `[uid, recurrence_id]` key of an occurrence change, `fields` every
changed field, and `root`, on an `aliased` line only, the resource that answers for this one. Read the current card for the new values.

## Examples

A catalogue of requests, one per value, covering every kind of card and source Pinakes handles:
the event pages of each platform, calendars and feeds, OSM in each accepted form, web pages,
media, books and articles in their spellings, social posts, Pubky objects of other apps, and the
`missing`, `unreachable` and `pending` states. Each is a named example in Swagger UI
(`/swagger-ui`), named by its value: pick one in the dropdown of a body or a parameter and use
**Try it out**. Every parameter of `GET /v1/search` starts at `(none)`; pick values in the
parameters you want (a map viewport sets `bbox` and `cluster`), and the others are
not sent. The paths are relative to the instance's base URL, for example
`curl "$BASE/v1/search?q=lugano"`; POST bodies are JSON
(`curl -H 'Content-Type: application/json' -d '{"uri":"…"}' "$BASE/v1/resolve"`). Resource IDs
are the deterministic hashes of the URIs, so the requests return data on any instance that
indexed them.

Times may be relative to the request: search's `from` and `to` and the change lookup's `since`
(a string, or an integer of Unix milliseconds) accept `now`, `now+30d` or `now-12h` (days or
hours, from the current minute), so the time windows and change lookups never go stale. Later pages
of one search resolve them at the instant its cursor pins, so the window does not move between
pages. In a raw query string `+` decodes to a space, and `now 30d` is accepted as `now+30d`.

Three caveats:

- Search lists only links someone referenced on Pubky, so the search examples need the URIs
  tagged; previews, cards, groups, dates and changes do not.
- The reach viewer is a placeholder; use a user that exists in the Nexus the instance reads.
- A group ID is the one a group of those members gets when it first forms; an instance where the
  group formed differently has another ID (take `group.id` from either member's card).

The examples live in `data/api-recipes.json`; `tests/recipes.rs` runs every one against a fixture
of the same URIs and checks that this list matches. `scripts/e2e --live-recipes` (also run by
`--isolated-live`) resolves each `/v1/resolve` example live and checks its card's type, so an
example whose page has gone stale fails there.

### POST /v1/resolve: a card per link

| Request | Card |
| --- | --- |
| `POST /v1/resolve {"uri":"https://luma.com/376whqyw"}` | Event |
| `POST /v1/resolve {"uri":"https://luma.com/je0oh6s1"}` | Event |
| `POST /v1/resolve {"uri":"https://www.meetup.com/bitcoin-meetup-switzerland/events/316899925/"}` | Event |
| `POST /v1/resolve {"uri":"https://www.eventbrite.com/e/se-bitcoin-sovereignty-economics-tickets-1978412197139"}` | Event |
| `POST /v1/resolve {"uri":"https://partiful.com/e/AK0FMV5lu2PmfSyxuXwY"}` | Event |
| `POST /v1/resolve {"uri":"https://posh.vip/e/halloween-2026-eden"}` | Event |
| `POST /v1/resolve {"uri":"https://events.ringcentral.com/events/innovators-retreat-2026"}` | Event |
| `POST /v1/resolve {"uri":"https://mobilizon.fr/events/86a665ae-e495-4e4f-88d4-4cffd44a82f1"}` | Event |
| `POST /v1/resolve {"uri":"https://events.sfconservancy.org/1ukbcQFkKAEr0OrZ7PDgq"}` | Event |
| `POST /v1/resolve {"uri":"https://www.petzi.ch/en/events/61387-fri-son-kim-wilde-uk/"}` | Event |
| `POST /v1/resolve {"uri":"https://eventyay.com/ev/88882f3e/"}` | Event |
| `POST /v1/resolve {"uri":"https://www.guidle.com/de/veranstaltungen/dies-das/ueberlingen/gregor-haegele-live-2027_A2rCGkH"}` | Event |
| `POST /v1/resolve {"uri":"https://allevents.in/zurich/200029487725505"}` | Event |
| `POST /v1/resolve {"uri":"https://www.fatsoma.com/e/npky3qjt/the-official-dublin-student-halloween-ball-2026"}` | Event |
| `POST /v1/resolve {"uri":"https://www.designmynight.com/london/whats-on/theatre/the-magic-hour-park-lane"}` | Event |
| `POST /v1/resolve {"uri":"https://fixr.co/event/halloween-2026-welcome-to-the-haunted-hotel-tickets-404456056"}` | Event |
| `POST /v1/resolve {"uri":"https://v4.wegow.com/es/conciertos/konzert-von-yasi-hofer-band-in-bonn-harmonie"}` | Event |
| `POST /v1/resolve {"uri":"https://grounds.stager.co/shop/default/events/111608505"}` | Event |
| `POST /v1/resolve {"uri":"https://www.songkick.com/concerts/42408878-bausa-at-komplex-457"}` | MusicEvent |
| `POST /v1/resolve {"uri":"https://events.humanitix.com/great-northern-contemporary-craft-fair"}` | Event |
| `POST /v1/resolve {"uri":"https://infomaniak.events/en-ch/festivals/festival-wfc-leman-2026/c90c619b-c155-454d-a277-c2e28e87d379/events/365609"}` | Event |
| `POST /v1/resolve {"uri":"https://xceed.me/en/roma/event/dissonanze-2026-day-1/220015"}` | Event |
| `POST /v1/resolve {"uri":"https://www.ents24.com/london-events/eventim-apollo/simon-brodkin/7474018"}` | Event |
| `POST /v1/resolve {"uri":"https://www.ticketino.com/en/Event/TICKETINO-BBQ/192364"}` | Event |
| `POST /v1/resolve {"uri":"https://api.btcmap.org/v4/events/1"}` | Event |
| `POST /v1/resolve {"uri":"nostr:naddr1qqjrvvp3vycxye33956nycec956x2ery94snqvfk95ergefnx9snqdeh8yekyq3qxfx8p5gzdyv0kgakc365pag8ej2p6g7l72k0hszt0g5m0k0wyymsxpqqqp7txuf3y56"}` | Event |
| `POST /v1/resolve {"uri":"nostr:naddr1qqjx2dryvcmn2c3395enwce4956rxeny943x2c35943kvetrvf3xycmyvvcrjq3qs0mv35668yg7t4jk49zdltkezn0em0s3jpt2qn8mkneurnw2edwsxpqqqp7tgmysyuc"}` | Collection |
| `POST /v1/resolve {"uri":"https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"webcal://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://www.calendarlabs.com/ical-calendar/ics/76/US_Holidays.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://ti.to/scipy/scipy2026.ics"}` | Event |
| `POST /v1/resolve {"uri":"https://pretix.eu/demo/democon/ical/"}` | Event |
| `POST /v1/resolve {"uri":"https://api.weezevent.com/sites/view/live/E1114506.ics"}` | Event |
| `POST /v1/resolve {"uri":"https://eventfrog.ch/stream/de/event/7503702322012243890.ics"}` | Event |
| `POST /v1/resolve {"uri":"https://bitcoinops.org/feed.xml"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://github.com/pubky/pubky-core/releases.atom"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://www.jsonfeed.org/feed.json"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/node/240109189"}` | Place |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/way/50637691"}` | Place |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/relation/146656"}` | Place |
| `POST /v1/resolve {"uri":"https://osm.org/node/240109189"}` | Place |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/edit?node=240109189"}` | Place |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/node/240109189/history"}` | Place |
| `POST /v1/resolve {"uri":"https://osm.org/go/0EEQjE--"}` | Place |
| `POST /v1/resolve {"uri":"https://www.openstreetmap.org/?mlat=46.0037&mlon=8.9511#map=16/46.0037/8.9511"}` | Place |
| `POST /v1/resolve {"uri":"geo:47.3769,8.5417"}` | Place |
| `POST /v1/resolve {"uri":"https://en.wikipedia.org/wiki/Bitcoin"}` | Article |
| `POST /v1/resolve {"uri":"https://en.wikipedia.org/wiki/Lugano"}` | Article |
| `POST /v1/resolve {"uri":"https://en.wikipedia.org/wiki/Immanuel_Kant"}` | Article |
| `POST /v1/resolve {"uri":"https://de.wikipedia.org/wiki/Immanuel_Kant"}` | Article |
| `POST /v1/resolve {"uri":"https://www.wikidata.org/wiki/Q90"}` | Place |
| `POST /v1/resolve {"uri":"https://n26.com/"}` | FAQPage |
| `POST /v1/resolve {"uri":"https://pubky.org/"}` | CreativeWork |
| `POST /v1/resolve {"uri":"https://casa.io/"}` | WebPage |
| `POST /v1/resolve {"uri":"https://example.org/"}` | WebPage |
| `POST /v1/resolve {"uri":"https://open.spotify.com/track/6LgJvl0Xdtc73RJ1mmpotq"}` | MusicRecording |
| `POST /v1/resolve {"uri":"https://music.apple.com/gb/song/1097861770"}` | MusicRecording |
| `POST /v1/resolve {"uri":"https://www.deezer.com/track/138539973"}` | MusicRecording |
| `POST /v1/resolve {"uri":"https://tidal.com/track/58990512"}` | MusicRecording |
| `POST /v1/resolve {"uri":"https://open.spotify.com/album/6dVIqQ8qmQ5GBnJ9shOYGE"}` | MusicAlbum |
| `POST /v1/resolve {"uri":"https://music.apple.com/gb/album/ok-computer/1097861387"}` | MusicAlbum |
| `POST /v1/resolve {"uri":"https://www.deezer.com/album/14879699"}` | MusicAlbum |
| `POST /v1/resolve {"uri":"https://radiohead.bandcamp.com/album/ok-computer"}` | MusicAlbum |
| `POST /v1/resolve {"uri":"https://open.qobuz.com/album/0634904078164"}` | MusicAlbum |
| `POST /v1/resolve {"uri":"https://music.youtube.com/playlist?list=OLAK5uy_nc6afxSnmK8OFML8dF9q_0D1MBMXsGnL4"}` | MusicPlaylist |
| `POST /v1/resolve {"uri":"https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb"}` | MusicGroup |
| `POST /v1/resolve {"uri":"https://www.youtube.com/watch?v=dQw4w9WgXcQ"}` | VideoObject |
| `POST /v1/resolve {"uri":"https://www.youtube.com/watch?v=Up2i2Z4m-Kg"}` | VideoObject |
| `POST /v1/resolve {"uri":"https://vimeo.com/76979871"}` | VideoObject |
| `POST /v1/resolve {"uri":"https://open.spotify.com/show/4rOoJ6Egrf8K2IrywzwOMk"}` | PodcastSeries |
| `POST /v1/resolve {"uri":"https://thebiz.pro/episodes/1/making-cypherpunk-products-and-businesses"}` | CreativeWork |
| `POST /v1/resolve {"uri":"isbn:9780140328721"}` | Book |
| `POST /v1/resolve {"uri":"isbn:0140328726"}` | Book |
| `POST /v1/resolve {"uri":"isbn:978-0-14-032872-1"}` | Book |
| `POST /v1/resolve {"uri":"https://openlibrary.org/books/OL7353617M"}` | CreativeWork |
| `POST /v1/resolve {"uri":"isbn:9780262033848"}` | Book |
| `POST /v1/resolve {"uri":"doi:10.1038/nature12373"}` | ScholarlyArticle |
| `POST /v1/resolve {"uri":"https://doi.org/10.1038/nature12373"}` | ScholarlyArticle |
| `POST /v1/resolve {"uri":"https://dx.doi.org/10.1038/nature12373"}` | ScholarlyArticle |
| `POST /v1/resolve {"uri":"https://www.nature.com/articles/nature12373"}` | ScholarlyArticle |
| `POST /v1/resolve {"uri":"doi:10.1038/nphys1170"}` | ScholarlyArticle |
| `POST /v1/resolve {"uri":"nostr:note1qqqdltcknk9rq52w5xq5srdx3znx4cvr4nmst5xz2rtwkm87ay9qeuwqtd"}` | SocialMediaPosting |
| `POST /v1/resolve {"uri":"pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json"}` | LearningResource |
| `POST /v1/resolve {"uri":"pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"}` | Conversation |
| `POST /v1/resolve {"uri":"https://www.mexc.com/"}` | `unreachable` |
| `POST /v1/resolve {"uri":"pubky://dir8gnjfnhdn5cpo8gxmfxfpqm9kffkoqpmsefcjkdpkxo15d5ey/pub/pinakes.e2e/none.json"}` | `missing` |
| `POST /v1/resolve {"uri":"https://www.privacyguides.org/dns/"}` | `pending` |

### POST /v1/resources/lookup: several cards

```text
POST /v1/resources/lookup {"uris":["https://luma.com/376whqyw","https://www.meetup.com/bitcoin-meetup-switzerland/events/316899925/","nostr:naddr1qqjrvvp3vycxye33956nycec956x2ery94snqvfk95ergefnx9snqdeh8yekyq3qxfx8p5gzdyv0kgakc365pag8ej2p6g7l72k0hszt0g5m0k0wyymsxpqqqp7txuf3y56"]}
POST /v1/resources/lookup {"uris":["https://www.openstreetmap.org/node/240109189","geo:47.3769,8.5417"]}
POST /v1/resources/lookup {"uris":["pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json","pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"]}
POST /v1/resources/lookup {"uris":["https://www.youtube.com/watch?v=dQw4w9WgXcQ","https://vimeo.com/76979871","https://www.youtube.com/watch?v=Up2i2Z4m-Kg"]}
POST /v1/resources/lookup {"uris":["isbn:9780140328721","doi:10.1038/nature12373","https://doi.org/10.1038/nature12373"]}
POST /v1/resources/lookup {"ids":["81cefcae5a6a468cfc27ff1c1b420fce","5bccc93f427b6fd44115fdd02618392f"]}
```

### GET /v1/search: one value per parameter

```text
GET /v1/search?type=Place
GET /v1/search?type=Event
GET /v1/search?type=MusicEvent
GET /v1/search?type=Book
GET /v1/search?type=ScholarlyArticle
GET /v1/search?type=Article
GET /v1/search?type=VideoObject
GET /v1/search?type=PodcastSeries
GET /v1/search?type=WebPage
GET /v1/search?type=SocialMediaPosting
GET /v1/search?type=DataFeed
GET /v1/search?type=CreativeWork
GET /v1/search?type=Product
GET /v1/search?type=Question
GET /v1/search?type=Recipe
GET /v1/search?type=JobPosting
GET /v1/search?remote=true&type=JobPosting
GET /v1/search?remote=false&type=JobPosting
GET /v1/search?closed=true&type=JobPosting
GET /v1/search?ingredient=mozzarella&type=Recipe
GET /v1/search?q=lugano
GET /v1/search?q=bitcoin
GET /v1/search?q=berlin
GET /v1/search?tags=bitcoin
GET /v1/search?tags=pubky
GET /v1/search?tags=lugano
GET /v1/search?tags=meetup
GET /v1/search?facet=tags
GET /v1/search?facet=type
GET /v1/search?facet=type,tags
GET /v1/search?near=46.0037,8.9511&radius=5000
GET /v1/search?near=47.3769,8.5417&radius=10000
GET /v1/search?near=52.5170,13.3889&radius=20000
GET /v1/search?from=now&to=now+1d
GET /v1/search?from=now+1d&to=now+7d
GET /v1/search?from=now+7d&to=now+30d
GET /v1/search?sort=start
GET /v1/search?sort=recent
GET /v1/search?sort=activity
GET /v1/search?sort=trending&window=24h
GET /v1/search?sort=trending&window=7d
GET /v1/search?sort=trending&window=30d
GET /v1/search?related=586d741bd42f9a0dde1c946b58db20ae
GET /v1/search?key=mbid:9f9cf187-d6f9-437f-9d98-d59cdbd52757
GET /v1/search?key=isrc:GBAYE9701376
GET /v1/search?key=upc:634904078164
GET /v1/search?key=wikidata:Q9312
GET /v1/search?key=isbn:9780140328721
GET /v1/search?key=imdb:tt0111161
GET /v1/search?key=podcast:304b84f0-07b0-5265-b6b7-da5cf5aeb56e
GET /v1/search?key=osm:node%2F240109189
GET /v1/search?key=gtin:0840440422266
GET /v1/search?collapse=group&type=Book
GET /v1/search?collapse=group&type=ScholarlyArticle
GET /v1/search?bbox=8.90,45.98,9.00,46.03&cluster=12
GET /v1/search?bbox=8.45,47.33,8.62,47.42&cluster=10
GET /v1/search?bbox=13.25,52.45,13.55,52.58&cluster=14
GET /v1/search?reach=following&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
GET /v1/search?reach=friends&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
GET /v1/search?boost=following&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
GET /v1/search?sort=trending&type=MusicRecording&boost=friends&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
```

### GET /v1/resources/{id}

```text
GET /v1/resources/81cefcae5a6a468cfc27ff1c1b420fce
GET /v1/resources/7f8f0e5f5b301b55a06d51df5d54281c
GET /v1/resources/2dea92aa8c3cf8ae1cffad92cf3ca718
GET /v1/resources/5bccc93f427b6fd44115fdd02618392f
GET /v1/resources/0f3820de1c3532e8805674cc996978f9
GET /v1/resources/9ed32f6665c475119517739f13a7d404
GET /v1/resources/586d741bd42f9a0dde1c946b58db20ae
GET /v1/resources/448cc746b24c989629ff12bff641f5ac
GET /v1/resources/4de689cf91b85eee5e7a5b6a16d719cb
```

### GET /v1/resources/by-uri

```text
GET /v1/resources/by-uri?uri=https:%2F%2Fwww.openstreetmap.org%2Fnode%2F240109189
GET /v1/resources/by-uri?uri=https:%2F%2Fdoi.org%2F10.1038%2Fnature12373
GET /v1/resources/by-uri?uri=https:%2F%2Fluma.com%2F376whqyw
GET /v1/resources/by-uri?uri=isbn:9780140328721
GET /v1/resources/by-uri?uri=pubky:%2F%2F47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao%2Fpub%2Fpubky-rooms%2Frooms%2F0035SFYCGVBV2
```

### GET /v1/groups/{id}

```text
GET /v1/groups/d1b4a3fefc115a5b7ea5bed6fb231011
GET /v1/groups/3089a188a2893b2477e80be72b8fdb8c
GET /v1/groups/722489247ea3402853880aea5ba198e0
GET /v1/groups/ca608de60e30acbafbdf80065b3dad2d
```

### GET /v1/resources/{id}/occurrences

```text
GET /v1/resources/2dea92aa8c3cf8ae1cffad92cf3ca718/occurrences?limit=20
GET /v1/resources/2950248974677c9ca34f1f67b8a2da80/occurrences?limit=10
```

### GET /v1/resources/{id}/episodes

```text
GET /v1/resources/faf081897e81377fd45b404f02a7f190/episodes?limit=20
```

### POST /v1/occurrences/lookup

```text
POST /v1/occurrences/lookup {"limit":20,"uris":["https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics"]}
POST /v1/occurrences/lookup {"limit":10,"uris":["https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics","https://luma.com/376whqyw"]}
POST /v1/occurrences/lookup {"limit":5,"resource_ids":["2dea92aa8c3cf8ae1cffad92cf3ca718"]}
```

### POST /v1/changes/lookup

```text
POST /v1/changes/lookup {"uris":["https://luma.com/376whqyw","https://www.meetup.com/bitcoin-meetup-switzerland/events/316899925/","nostr:naddr1qqjrvvp3vycxye33956nycec956x2ery94snqvfk95ergefnx9snqdeh8yekyq3qxfx8p5gzdyv0kgakc365pag8ej2p6g7l72k0hszt0g5m0k0wyymsxpqqqp7txuf3y56"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["https://www.openstreetmap.org/node/240109189","geo:47.3769,8.5417"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json","pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["https://www.youtube.com/watch?v=dQw4w9WgXcQ","https://vimeo.com/76979871","https://www.youtube.com/watch?v=Up2i2Z4m-Kg"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["isbn:9780140328721","doi:10.1038/nature12373","https://doi.org/10.1038/nature12373"]}
POST /v1/changes/lookup {"resource_ids":["81cefcae5a6a468cfc27ff1c1b420fce","5bccc93f427b6fd44115fdd02618392f"],"since":"now-7d"}
```

## Search

| Parameter | Meaning |
| --- | --- |
| `q` | Full-text name/description search with a trigram/partial-name fallback; at most 256 bytes. A post whose author opted out of search indexing (a read's `facts.opted_out`: Mastodon's `indexable: false` or `noindex`, Bluesky's `!no-unauthenticated`) never matches |
| `type` | A schema.org type, ancestors included (`Event` matches `MusicEvent`; `Place` matches cafés and other `LocalBusiness` types), or up to 10 comma-separated: any of them (`type=MusicRecording,MusicAlbum,MusicGroup`). An unknown type is a 400 that names it (only a name of letters and digits is repeated; anything else, such as a URL, gets a general message) |
| `category`, `host` | Exact filters |
| `key` | Links carrying one exact identifier, on any service and in any spelling: `mbid:{uuid}` (a MusicBrainz recording, release, release group or artist), `wikidata:{Q-id}`, `isrc:{code}` (any letter case, hyphens allowed), `upc:{barcode}` (a UPC, EAN or GTIN in any of its lengths, check digit verified), `isbn:{ISBN-10 or ISBN-13}` (an edition), `imdb:{tt…}` (a film or series), `podcast:{GUID}` (a show), `podcast:enclosure:{URL}` (an episode, by the file it plays), `osm:{node|way|relation}/{id}`, `doi:{DOI}`, `gtin:{barcode}` (a [product](#products), in any of its lengths), any key exactly as a resource read lists it in `keys` (`mb:recording:{mbid}`, `podcast:guid:{GUID}`, `podcast:apple:{id}`, …), or another namespace a source records (its page in [docs/sources](sources/README.md#also-on) names its namespaces and how each is normalized), as the [also-on](#also-on) sources found them as keys or facts (`mbid:` also the MBID a MusicBrainz page was merged from, its `mbid_was` fact, and with `type=MusicEvent` an artist's events, by their performers), or as an exact lookup by that key named the link; anything else is 400 |
| `related` | A resource ID: what the people who tagged it also referenced, ranked by how many of them did ([Related](#related)); not with `sort` |
| `tags` | Comma-separated labels; at most 5, each 1–128 bytes |
| `match` | `any` (default) or `all` (at most 3 tags) |
| `author` | Resources referenced by this 52-character z-base-32 public key |
| `reach`, `viewer` | `following`, `friends` or `wot_2` of the Nexus user `viewer`: only their references and tags count (a filter) |
| `boost`, `viewer` | `following`, `friends` or `wot_2` of the Nexus user `viewer`: cards they referenced rank higher, the more of them and the more recently they did, and nothing is left out ([Reach](#search)); with the default ranking or `sort=trending` |
| `venue` | Events at this venue resource ID |
| `remote` | `true`: [job postings](#jobs) that may be done remotely; `false`: job postings that may not |
| `closed` | `true`: list [job postings](#jobs) past their closing date too; they are left out otherwise |
| `ingredient` | [Recipes](#recipes) with these ingredients: up to 3, comma-separated, each of letters, digits, spaces and hyphens, matching whole words of an ingredient's name as the card records it (`flour` matches "all-purpose flour", `mozz` matches nothing); all must match |
| `sort` | `activity` (default), `recent`, `distance` (requires `near`), `start` or `trending` (by the references made inside `window`; only links referenced in it; with `collapse=group` one song across services is one row, ranked by the references all its links got in the window) |
| `window` | With `sort=trending` or `related`: `{n}h` or `{n}d`, from `1h` to `90d` (default `7d` for trending, `limits.related_window_days` for related), counted back from the first page's instant |
| `limit` | 1–500, default 50 |
| `cursor` | The signed next-page token of the previous page |
| `facet` | `tags`, `type` or both (`type,tags`): counts over the whole filtered set, in `facets.tags` (`{label, count}` per label) and `facets.type` (`{type, type_path, count}` per card type, `type_path` from `Thing` down to the type: enough to build the type tree and roll counts up into families), most frequent first |
| `bbox` | `minLon,minLat,maxLon,maxLat` in degrees; west > east crosses the antimeridian |
| `near`, `radius` | `lat,lon` and a radius in metres (at most `limits.max_radius_m`, default 200 km); not with `bbox` |
| `from`, `to` | Inclusive start-time bounds: RFC 3339, Unix milliseconds or relative (`now`, `now+30d`, `now-12h`), within UTC years 0001–9999 |
| `cluster` | A map zoom, with `bbox`: returns clusters and points instead of a page |
| `collapse` | `group`: one row per group, after filtering members (a list view; not with `cluster`) |

Unknown parameters, invalid coordinates or dates, an out-of-range radius, a non-finite zoom and
incompatible options return 400 (`timestamp out of range` for a date outside years 0001–9999).
Search lists only resources with at least one public reference.

**Events.** `type=Event` with `from`/`to` and `sort=start` covers Event pages with a start,
calendar occurrences and located Pubky event references. Each of these is one result unit, so
a calendar with three matching dates counts three times. `start` sorts exclude units without a
start. A Pubky event result carries `event` for Nexus post hydration and no name: Pinakes keeps
the post's time, status and place, not its text. Author and reach filters also constrain that
post's author.

**Jobs.** `type=JobPosting` with `near` or `bbox` finds postings by their workplace,
`remote=true` the remote ones, and `from`/`to` select by the date posted (a posting's `start`).
A posting past its closing date is left out of every search unless it sets `closed=true`
([Jobs](#jobs)).

**Ranking.** `activity` combines distinct taggers and references with a 30-day decay:

```text
(log(1 + taggers) + 0.5*log(1 + replies) + 0.3*log(1 + embeds + collections))
  * 2^(-age_of_newest_reference_in_days / 30)
```

`recent` ranks by the newest reference; `distance` and `start` by negative metres and negative
Unix seconds, so nearest and earliest come first. Scores are internal and never returned.

**Paging.** Pages are keyset-ordered on rank, then one internal key per resource or occurrence. The cursor is signed with a key created at
daemon startup (HMAC-BLAKE3) and binds the filters, sort and decay instant; keep them unchanged
(`limit` may change). A tampered cursor, a cursor for other filters or one from a previous
daemon returns 400: restart without a cursor. A null cursor ends the results. Concurrent
updates are visible between pages; this is not a frozen snapshot.

**Map clusters.** `cluster=<zoom>` returns `{resolution, clusters, points, truncated, total,
reach_truncated, facets?}`. A cluster is `{cell, count, lat, lon, top, attribution?}`, where
`top` holds at most three card IDs by rank, `lat` is the members' mean latitude and `lon` the
circular mean of their longitudes. Points are cards. The H3 resolution comes from `limits.h3_resolutions` by floored zoom;
large viewports coarsen to stay within `limits.max_cluster_cells` and `resolution` reports the
value used; a viewport that still has more cells returns 400. Singleton cells are points, and when the filtered total fits in `limit` every unit
is a point. `limit` caps clusters plus points; `truncated` marks omitted units, and otherwise
the cluster counts plus the points equal `total`. Cluster mode has no cursor: refine the
viewport.

**Groups.** Without `collapse`, every member of a group is a row of its own, ranked on its own
references. `collapse=group` filters first, then keeps one row per group: the matching member
with the most references, counted up to `limits.collapse_refs_per_member` (10,000) each (ties
by the lower ID), as its own card, ranked as itself. Facets count
every matching link either way; clusters always count links, so `collapse` is refused with
`cluster`.

**Reach.** Following and friends lists come from Nexus in pages of 200, at most 1,000 authors,
cached per viewer for five minutes. `wot_2` (two steps of the follow graph) comes from Nexus's
reach-set route, `GET /v0/user/{viewer}/reach-set?depth=2` (`{authors, truncated}`, at most 1,000,
with an `ETag`), only where the operator says the Nexus serves it (`nexus_reach_set = true`;
else `wot_2` answers 501): it is kept five minutes, then asked again with its `ETag` and kept
while Nexus answers 304. `reach_truncated: true` warns that the `reach` or `boost` set was
capped. All pages share `limits.reach_timeout_secs` (5 s); an incomplete list is not cached. An
unknown Nexus user has an empty reach; a Nexus error or the deadline returns 502. Sets live in the
daemon's memory only; nothing per viewer is stored.

`reach` filters: only references by authors in the set count, for the rows and every count.
`boost` ranks and never filters: with the default ranking a card gains `2 · ln(1 + n)` for the
`n` authors in the set who referenced it, halved every 30 days since the newest of their
references (the card's own score decays the same way, and one recent reference from the set
weighs about as much as three taggers in it); with `sort=trending` a reference in the window by
an author in the set counts twice. `boost` takes no other `sort` and no `related`. Both may be
given, each with its own set.

**Caching.** Responses carry `Cache-Control: public, max-age=30` and an `ETag` over the
parameters, the resolved time window, the ingestion cursors and epochs, a write revision, the
same-event revision and the reach and boost sets' hashes and truncation; `If-None-Match` returns 304 when nothing changed.

## Related

`related={resource id}` lists what the people who tagged a resource also referenced: other
resources, ranked by how many of its taggers referenced them, then by the newest of those
references. It is what a "Fans also tagged" list or a "Because you tagged X" row needs, and like
every filter it combines with `type`, `tags`, `q`, `reach`/`viewer`, `collapse=group` and `facet`
(not with `sort`: it is the ranking).

Related lists only what at least two of a link's taggers referenced, so it never mirrors one
person (`limits.related_min_shared`, 2).

- **Groups.** A resource in a [group](#groups) stands for the whole group: its taggers are the
  group's members' taggers, and the group's own members are never listed. With `collapse=group`,
  each row is one group, its members' taggers counted together; without it, each resource counts
  its own.
- **Bounds.** The taggers are drawn from a sample of the resource's most recent
  `limits.related_tag_sample` (2,000) tags within `limits.related_window_days` (365, or
  `window`): one sample per member of its group and per alias of each, up to
  `limits.related_seed_links` (32) of those links, the ones with the newest tags; all taken
  before `reach` applies and never filled back up when `reach` leaves fewer; of the sample's
  authors in reach, the `limits.related_taggers` (200) with the newest tag are sampled. Of each,
  only their most recent `limits.related_refs_per_tagger` (1,000) references within the same days
  count. With `collapse=group`, a group needs the shared taggers of its members that pass the
  other filters, together. `/v1/info` gives every limit here. A related search costs
  `limits.related_cost` (6) units of the [per-IP budget](#limits-and-errors).
- Only the references Pinakes copies from Nexus's feed count: the resource's taggers, and any
  reference of theirs (a tag, a reply, an embed, a collection, an event). With `reach`, only the
  taggers within the viewer's reach.
- Each resource is listed once, as its own card: never once per occurrence of a feed or per
  event reference at a venue.
- A post whose author opted out of indexing is never listed, as it is never found by its text,
  and references to it count for nothing: not toward its group, and its taggers are not the
  taggers of a group it is in.
- An alias reads as its root; an unknown resource relates to nothing.

## Resolution and status

`POST /v1/resolve` queues a supported URI and waits up to three seconds. Ingestion and every
preview caller share one queue entry per normalized URI, with one TTL, retry schedule and
dead-letter state. A 202 carries the ID to poll with `GET /v1/resources/{id}`. Unsupported
schemes or disabled resolvers return 400. A resource only previewed, never referenced, is not
a search result and expires after the orphan grace period. Previews are limited per client IP
and per URI (`limits.preview_requests_per_minute`, `limits.preview_uri_requests_per_minute`,
30 each); a 429 carries `Retry-After: 60`.

Statuses:

- `ok`: the current metadata was accepted.
- `pending`: queued and not yet resolved. A link's first missing answer leaves it `pending` until a
  second fetch confirms it: one with no accepted card yet (a new preview) is asked again after
  `limits.backoff_secs` (30 s), and `POST /v1/resolve` answers 202 with `Retry-After` until then.
- `missing`: confirmed gone by two fresh fetches; one missing answer keeps the last accepted
  metadata (the second fetch comes after `limits.missing_ttl_secs`, at least an hour), and a
  repeated cached observation does not count twice.
- `unreachable`: the upstream denied access (401, 403) or answered with a challenge page (a bot
  wall, captcha or sign-in redirect); it is retried with backoff like an error.
- `error`: the fetch failed; it is retried with backoff and dead-letters after
  `limits.max_failures`.

A failed refresh keeps the last accepted metadata.

## Aliases

An alias is the same object under another URI. It keeps its own resource ID (an ID never
changes), but it is never fetched: its references count for its root, search lists only the
root, and a read, lookup, change query or occurrence read of the alias answers with the root
(so the card's `id` is the root's). Aliases are resolved deterministically:

- **Tracking parameters**: `utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `igshid`, `mc_cid`,
  `mc_eid` and `_gl` on every host, `recId`, `recSource`, `searchId` and `eventOrigin` on
  meetup.com and `tk` on luma.com and lu.ma are dropped from the raw query (every other value
  keeps its encoding); a URI that had any is an alias of the URI without them.
- **OSM spellings and object shortlinks** are aliases of `https://www.openstreetmap.org/{type}/{id}`.
- **Identifiers**: an ISBN-10 or hyphenated ISBN is an alias of `isbn:` and its ISBN-13;
  `https://doi.org/…`, `https://dx.doi.org/…` and any letter case are aliases of `doi:` in
  lower case.
- **Official-API spellings**: every address of an object a [source](#sources) reads is an
  alias of one root, e.g. each Federal Register address of a document is an alias of
  `fr:{number}`. When the API itself names another root for the object, the link becomes an
  alias of that root after its first fetch.
- **Cursor feed variants** (BTC Map's delta feed with parameters, the portal's endpoints) are
  aliases of their feed.
- **Music service links**: every spelling of one track, album, artist or playlist on one service
  is an alias of its canonical URL: Spotify (`intl-xx` paths, `spotify:{kind}:{id}`, the old
  `user/…/playlist/…` form, `play.spotify.com`, the embedded player's `/embed/…` and the retired
  `embed.spotify.com/?uri=spotify:…`) → `https://open.spotify.com/{kind}/{id}`; Apple Music →
  `https://music.apple.com/{kind}/{id}` without the storefront (the catalogue ID is global, and
  Apple answers the storefront-less URL with the viewer's storefront) and without the name slug,
  an album URL with `?i={id}` naming that track (`/song/{id}`), the iTunes Store's
  `itunes.apple.com/…/id{N}` too; YouTube and YouTube Music (`youtu.be`,
  `m.`/`music.youtube.com`, `/shorts/`, `/embed/`) → `https://www.youtube.com/watch?v={id}` and
  `…/playlist?list={id}`; Deezer without the language path → `https://www.deezer.com/{kind}/{id}`;
  TIDAL (`/browse/`, `listen.tidal.com`, the `/u` share suffix; tracks, albums, artists, videos, playlists) →
  `https://tidal.com/{kind}/{id}`; Qobuz store album
  pages and `play.qobuz.com` → `https://open.qobuz.com/{kind}/{id}`; SoundCloud (`m.`, any letter
  case, parameters) → `https://soundcloud.com/{user}`, `…/{user}/{track}` or `…/{user}/sets/{set}`; Amazon Music (`albums/{id}?trackAsin=` naming that
  track; Amazon's own hosts only, `music.amazon.com`, `.co.uk`, `.de`, `.fr`, `.it`, `.es`, `.ca`,
  `.com.au`, `.co.jp`, `.in`, `.com.br`, `.com.mx`) → `https://music.amazon.{tld}/{kind}/{id}`;
  Bandcamp without parameters; Discogs (the name after the ID, a language path, no `www`) →
  `https://www.discogs.com/{kind}/{id}`. Short links
  that need a request to expand (`spotify.link`, `deezer.page.link`, `on.soundcloud.com`) are not
  folded.
- **Wikidata items**: `wikidata.org`, `m.wikidata.org`, `/entity/Q…` and
  `/wiki/Special:EntityPage/Q…` are aliases of `https://www.wikidata.org/wiki/{Q-id}`.
- **Wikipedia articles**: the mobile host, `index.php?title=…` (a revision with `oldid` too),
  spaces and percent-encoding are aliases of `https://{lang}.wikipedia.org/wiki/{Title}`. A
  redirect's title stays its own link.
- **MusicBrainz pages**: a recording, release, release group or artist page, or an ISRC's page,
  on `musicbrainz.org`, `www.` or `beta.`, over http or https, its web service record
  (`/ws/2/…`), the MBID in any letter case, a trailing slash and parameters are aliases of
  `https://musicbrainz.org/{kind}/{mbid}` or `https://musicbrainz.org/isrc/{ISRC}`.
- **The page's own canonical**: after a fetch, a page whose head canonical link or
  all-permanent redirect chain names a URL that differs from it only by a trailing slash and tracking
  parameters becomes an alias of that URL. A canonical to another path (a homepage, a sibling
  page), another host or another query (`/events/?event_id=42` naming `/events/`) never
  aliases.

Refresh follows the time to an event's start: more than 30 days or no start, 7 days; 7–30 days,
2 days; 24 hours to 7 days, 6 hours; under 24 hours or running, 1 hour
(`limits.refresh_resource_secs`). Calendars and feeds use `limits.refresh_container_secs` (1
day, 12 hours, 6 hours, 1 hour), with the feed's own TTL in place of the first two; cursor feeds
refresh every `limits.refresh_cursor_secs` (5 minutes). Every other interval is at least an
hour. Adapter floors can lengthen these; OSM follows `limits.osm_ttl_secs`. One fetch at end + 24 hours (start + 24 hours without
an end) makes a resource final. A final resource stays frozen until a reference newer than its
last resolution reopens it; `geo:` points stay final.

## Changes

`POST /v1/changes/lookup` answers what changed for the links a client keeps: a page
`{lines, next_cursor, has_more, oldest_cursor}` of [change lines](#change-line), ascending by
`seq`, at most `limit` (1–500, default 100). Continue with `next_cursor`; when caught up it
advances to the head even if nothing matched. A nonzero `cursor` older than the retained history
returns **410**: restart without a cursor and refresh the resources you keep.

It takes `resource_ids`, `uris` or both, 1–500 together, and an optional inclusive `since`. A
link is read as `POST /v1/resources/lookup` reads it (normalised; an alias spelling is the
alias, whose line names its root and whose changes include the root's), so the same `uris` list
works for both lookups. Without `since` it answers every retained change of those
resources, in pages. `since`, when given, is RFC 3339, Unix
milliseconds (integer or text) or relative (`now-30d`), within UTC years 0001–9999. A relative
`since` is fixed by the first page: later pages (with `cursor`) keep that window, capped at the
cursor line's time less one second, so lines written between page fetches are not skipped.
Prefer `cursor`; when you use `since`, reuse a previous line's `at` and deduplicate by `seq`.
Unknown IDs and links add nothing. History outlives orphan expiry.

Classes are `created`, `rescheduled` (start or end), `relocated` (location or venue),
`cancelled`, `uncancelled`, `renamed`, `details`, `deleted` and `aliased`. When several fields
change at once one class is chosen in this order: lifecycle, time, location, name, details;
`fields` still lists them all. Orphan expiry emits `deleted`. A resource that becomes an
[alias](#aliases) emits `aliased` with `fields: ["root"]` and `root`, the ID that answers for it
from then on: its own ID when an alias answers for itself again (no rule names its root any more,
after an upgrade's `reextract`); the changes of an alias's ID are its own and its root's. A fetch failure emits nothing and never cancels a resource.
A calendar snapshot with more than `limits.feed_max_changes` (100) occurrence changes emits one
feed-level `details` line with `fields: ["occurrence_changes"]` (and `"resource_change"` when
the feed's own card changed too): re-read the occurrence window.
Lines are kept `limits.change_retention_days` (90) and pruned hourly.

## Calendars and occurrences

`GET /v1/resources/{id}/occurrences` returns a feed's accepted occurrences ordered by start,
UID and recurrence key; superseded rows are excluded. `from` defaults to the start of the last
accepted horizon; an earlier explicit bound reads retained older rows. The signed `cursor`
fixes the resource and window and expires on daemon restart; a tampered or mismatched one is
400. A known resource that is not a calendar returns an empty list. ICS feeds, event platform
feeds, BTC Map, the Google Calendar API and NIP-52 calendars all produce the same occurrences.

`POST /v1/occurrences/lookup` reads several feeds' occurrences as one page, ordered by start,
then feed, UID and recurrence key; each card's `id` names its feed. The feeds are
`resource_ids`, `uris` or both, 1–500 together, a link read as `POST /v1/resources/lookup`
reads it. The window, `limit` and cursor work as on the single feed, with `from` defaulting to
the earliest horizon start among the feeds; the cursor is bound to the resolved feeds (an alias
as its root), in any order and however they were named. An alias reads its root; an unknown ID or link adds nothing.

**Times.** An offset-only timestamp keeps its instant. Floating times take the TZID,
`X-WR-TIMEZONE`, `event.default_timezone` or the coordinates' zone. Recurrence follows RFC
5545 across DST changes: an ambiguous wall time (the repeated autumn hour) occurs once, at its
first instance; a nonexistent spring time moves forward by the gap. A series whose DTSTART is
UTC (`…Z`) expands in UTC unless `event.utc_series_wall_clock` selects a zone source: the
default `x_wr_timezone` takes an explicit `X-WR-TIMEZONE` (Google exports UTC DTSTARTs with a
local rule) or a TZID, `coordinates` also takes the default and coordinate zones, and `never`
keeps UTC. A UNTIL is compared as an instant. Floating RECURRENCE-ID, RDATE and EXDATE values
resolve in the series' zone. A series whose embedded VTIMEZONE cannot convert its DTSTART
expands in UTC with the warning `series_zone_unconvertible`. All-day ends are exclusive dates.

**Bounds.** Event years have four digits (RFC 5545 `date-fullyear`). Stored instants are
limited to UTC years 0001–9999 and truncated to microseconds; a leap second rolls into the next
second. An occurrence starting outside that range is dropped (`occurrence_start_out_of_range`),
and an end or modification time outside it is dropped from its item, so one bad item never
fails a feed. Recurrence expansion, components, revisions, lines, body size and parse time are
capped by the `event_*` limits. A recurrence rule is expanded only when it repeats yearly,
monthly, weekly, daily or hourly and one of its periods holds at most `event_max_period_dates`
(100,000) dates: the days the period keeps after the rule's day filters, times its listed
hours, minutes and seconds. Any month-day list counts as the whole month (31 days), the rule's
own or the one taken from DTSTART, so a plain monthly or yearly rule counts 31. An event with
another rule keeps DTSTART and its RDATEs, less its EXDATEs, within the horizon, with the
warning `series_rule_unexpanded`; a VTIMEZONE with one cannot convert, and an event in that
zone is left out with a warning. An event with more than one RRULE, or with an EXRULE, is left out
with a warning.

**Changes and disappearance.** Rows are compared by SEQUENCE, LAST-MODIFIED and a normalized
content hash, so DTSTAMP and fetch noise create no changes. Explicit cancellations apply at
once; disappearance needs two fresh complete snapshots, and roll-off past the horizon or past
end + 24 hours never cancels. A new UID with identical content supersedes the old row without a
cancellation. A snapshot that loses more than half its active keys, or adds more than
`limits.feed_max_new_keys` (1000) to a feed with rows, is held until an identical snapshot
confirms it; a feed over `limits.feed_max_rows` (10000) is held until it shrinks, and
`limits.feed_max_holds` (5) consecutive holds dead-letter it. A re-parse that moves instances
(for example by an hour across a DST change) re-keys them once, under the same rules. A feed
answering 404, 410 or 451, or with an empty feed, counts as an empty snapshot: two of them
cancel its rows and mark it missing, while its history stays readable here. A feed that answers
HTML instead is unreachable (retried with backoff) and keeps its rows. Cancelled, superseded and past rows are purged
`limits.feed_retention_days` (30) after their end.

Calendar validators (ETag, Last-Modified) are sent only after a saved snapshot with nothing
held or pending and more than half the forward window left; otherwise an unconditional GET
fetches a body. A 304 renews the TTL but neither confirms a disappearance nor extends the
horizon. Mixed dated and undated RSS, Atom and JSON feeds keep their dated items.

**Pubky events.** Social events and rsvps (pubky-social-specs v1.1,
`pubky://<author>/pub/social/v1/posts/{id}`) are Nexus's: Nexus indexes their time, occurrences,
rsvps and counts, and a client reads them there. Pinakes never fetches or indexes them; it
indexes external events (pages, calendar feeds, NIP-52), and a client merges both. The resource
feed carries no event lines: a `kind:event` line is skipped as `unknown_kind`, a `post` line for
a social post is rejected. Event references (one per occurrence and place, the `event` field)
remain in the schema and the search, but nothing writes them today.

**NIP-52 calendars.** A calendar waits (`calendar_members_pending`) for members not yet
resolved, at most `limits.calendar_member_wait_hours` (48) after it first listed them. A member
still unresolved then, one whose resolution failed, or one without a start is skipped with
`calendar_member_unavailable`: the other members apply, and the skipped member's rows are
cancelled after two snapshots. A calendar's link to a member it no longer lists ends at once,
and a listed member's link lives 24 hours, renewed at every fetch of the calendar, so a calendar
that rotates its members never runs into its cap and the links of one nobody fetches lapse.

## Episodes

`GET /v1/resources/{id}/episodes` lists a podcast show's latest episodes as its feed lists them,
newest first, at most 20 (`limit`, 1–20): `{items: [...]}`, each item
`{uri, name, image, published, guid, stream, id?}`. `guid` is what names the item: its GUID as
the feed gives it; its page when the feed gives none; else `sha256:` and the hex digest of its
enclosure's URL (without a fragment), so a link that carries a token shows nothing of itself.
`uri` is the episode's page (the feed item's link), else its feed item (`{feed}?guid={guid}`);
the resolver names items the same way, so either resolves to the episode's `PodcastEpisode` card
(`POST /v1/resolve`), and `id` names that card when Pinakes already has it (an alias as its
root). `stream` is the item's enclosure when it is public and direct, in the
same shape as a card's (`{url, type, duration_s, preview}`: the URL a client hands to an audio
element, its `audio/*` or `video/*` type, its length in whole seconds when the feed states one,
and `preview: false`, the whole episode); `null` when the item has no enclosure or a signed,
expiring, credentialed or non-media one (the rules of a card's `stream`). An entry is not a card:
nothing in it is tagged, grouped or counted.

The list is read with the show: the feed's own card (`PodcastSeries`) takes it from the feed it
is; a Podcast Index or Apple Podcasts show page (and the player pages that open a show by its
Apple ID) fetches the feed its record names, once per read of the show, and takes the same list.
It is replaced whole each time the show's card is saved from a read of its feed; a read that
could not reach the feed, or not read and parse it within three seconds and what is left of the
read's own time, keeps the list it had (the list is optional: a slow feed never costs the show
its card). A card replaced without a read (cleared after an error, scrubbed) lists nothing. An
item's `stream` is `null` as well when its stored form would not fit the store's bound (a URL
near 2,048 bytes that JSON escaping doubles): the episode is listed all the same. A show card without a list of its own (a page
whose feed could not be read, a service that names no feed) answers with the list of a member
of its [group](#groups) that has one: the same show elsewhere. A known resource that is no
show, or whose feed was not read yet, returns an empty list; an unknown ID is 404. The items a
feed holds beyond its first 200 are not looked at: feeds list newest first, and a few are out of
order. Why a route and not a fact: a fact is one string per name, and a card's document is
bounded; why not occurrences: an episode has no instant to window by, no recurrence and no
document of its own here.

```text
GET /v1/resources/{id}/episodes?limit=20
```

## Groups

Spellings of one object are [aliases](#aliases), one resource. A group joins resources that are
one thing, only on evidence that cannot be spoofed:

- **An identifier and its landing page**: a DOI or ISBN card and a page its registry record
  names as the landing page, when that page cites the identifier in its head (`citation_doi`,
  `prism.doi`, `dc.identifier`, `bepress_citation_doi` or `citation_isbn` meta). Open Library's
  own edition page needs no citation, and joins only an ISBN card naming it (two cards naming
  one edition page join only through the indexed page). A doi.org URL is the DOI itself, never a landing page.
- **One ICS UID within one feed**, between dated instances that start within five minutes of
  each other. The UID is hashed into the key, so no UID text names another feed. Across feeds only with `groups.cross_container_occurrences = "corroborated"`: both
  located within 500 m, starting within 300 s, with a name similarity of at least 0.5.
- **An exact key a source vouches for** ([Also on](#also-on)): two links a hub names as the same
  recording, release or Wikidata item, by its own identifier (`mb:recording:{mbid}`,
  `wikidata:Q…`), or a known music service's own record of its own object (the ISRC on a TIDAL
  track page). Any other page's claims are never such a key. A group holds at most one value of
  each single-object namespace: MusicBrainz recording, release, release group and artist
  (`mb:…`), Wikidata item (`wikidata`), IMDb title (`imdb`), Open Library edition and work
  (`openlibrary:…`), podcast GUID and Apple podcast ID (`podcast:guid`, `podcast:apple`), OSM
  object (`osm`), DOI (`doi`), and the namespaces each source's page in
  [docs/sources](sources/README.md#also-on) calls single-object: a join that would bring a second (a wrong ID on a hub) is refused,
  so one bad claim never chains two groups. The database remembers every such namespace a
  source registered (`single_object_namespace`), so a removed source's keys keep the rule until
  they expire. ISRCs, barcodes, ISBNs and other keys may be several. An event page's keys are never such
  evidence: a concert's performer key (`mb:artist:…`) names the artist, not another copy of the
  concert, so events group only with events.
- **Curation** on the admin listener.

`sameAs`, canonical and other URL claims, and other identifiers, never group. Two located
members more than `limits.group_max_distance_m` (500 m) apart never merge automatically; a group
holds at most `limits.group_max_members` members; points never group. Singletons stay
individual. A grouping failure backs off on its own and never blocks a save.

An automatic group's ID is stable once formed: it stays as members join, a merge keeps the ID
most members carry, and after a split the larger part keeps it (a new group is named after its
smallest member, and never takes an ID an existing group has). Member IDs and references never
change. A group is a hint, never a merge: nothing about its members is combined or stored with
it, so each keeps its own card, references, score and ranking, and the group tables hold only
membership (no OSM-derived value can reach them).

On the **admin listener**, `PUT /v1/groups/{id}` replaces a group's member set and answers as
`GET /v1/groups/{id}`:

```http
PUT /v1/groups/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Authorization: Bearer <admin-token>
Content-Type: application/json

{"members":[{"resource_id":"0123456789abcdef0123456789abcdef","occurrence":null}]}
```

An occurrence member is `{"uid":"calendar-uid","recurrence_id":"2026-10-20T17:00:00Z"}`.
Members must exist and have been grouped once. Curation pins its members away from automatic
groups and overrides the distance and evidence rules; removing a member lets its automatic
evidence count again. Invalid curation returns 400; a busy group writer returns 503 (retry). `groups.enabled = false` turns off group
reads, writes and collapse; stored membership stays for when it is turned back on.

## Possibly the same event

One meetup is often listed on several sites (Meetup, Luma, its organiser's page). These pages
share no identifier, so they never [group](#groups). Instead an event's card names other events
that are possibly the same one, in `maybe_same`: `{id, uri, site, start, score}`, best first, up
to three on a search, lookup or group card and ten on a resource read. An entry is an event resource, or one occurrence of
a feed (`id` is then the feed's ID and `uri` the occurrence's own page, else the feed's; a feed
read from a service's API is named by that service's page, never the API endpoint).

Two events are possibly the same when all of these hold:

- they start within 300 seconds of each other; when either is an all-day event, they start on
  the same local date (each in its own zone);
- when both are located, within 500 m of each other; when one or both have no coordinates,
  they name the same venue (its resource or OSM object) or the same organizer URL. So one
  organizer's meetups in two cities are not the same when both are located, but an event without
  coordinates pairs with the organizer's other event at that time and under a similar name,
  wherever that one is;
- their names are at least 0.5 alike (pg_trgm similarity) once both are normalised: lower case;
  without dates, times and years (`21.10.2026`, `2026-10-21`, `6.11.`, `21.`, `19:00`, `7pm`,
  `2026`, a month name with its day such as `Oct 21` or `6th November`, in English, German,
  French and Italian); without punctuation, symbols and emoji; without weekday names and filler
  words (`meetup`, `event`, `edition`, `session`, `monthly`, `weekly`, `stammtisch`, `treffen`,
  `rencontre`, `incontro`); a name with nothing left never matches;
- other numbers stay in the name (`#50`, `Nr. 30`, `Room 101`), and when both names keep
  numbers, they are the same numbers: "Meetup #50" and "Meetup #51" are not the same event;
- they are on different pages: two occurrences of one feed never match.

An event without coordinates, a venue or an organizer URL is therefore never listed: for
example a calendar entry without `GEO` whose `LOCATION` is one line of text (only a structured
street address is geocoded).

`score` is the name similarity, from 0.5 to 1. A member of the card's own group, an alias and a
gone occurrence are never listed.
It is a hint, like a group, and less: nothing is merged, counted or ranked by it, and each event
keeps its own card.

A background pass computes the pairs for events starting from a day ago to a year ahead, again
whenever an event's start, place or name changes; a request only reads them. Each event keeps
its best twenty pairs and is named by at most forty others, so a change marks a bounded number
of events to refill their lists; a read examines at most a hundred candidates, and one that runs
out with room left goes on from where it stopped in a later read. A new or changed
event is paired within about five seconds of its save, once the events before it in the queue
are read; an hourly sweep, which also runs at startup, queues events that entered the window or
changed unseen. Search
pages change their `ETag` when a hint on them changes. `[event] same_event_hint = "off"`
([operations](operations.md#calendars-and-events)) removes the field from event cards and details
and stops the pass (and from the OpenAPI document, with the recording hint off too).

## Possibly the same recording

One recording is often on a page nothing exact joins to the others: a re-release whose ISRC
MusicBrainz does not know, a service page that states no identifier. Such pages never
[group](#groups) (a group needs a key a hub or the service vouches for). Instead a
`MusicRecording` card names other recordings that are possibly the same one, in `maybe_same`:
`{id, uri, site, start, score}` as for events, with `start` `null`, best first, up to three on a
card and ten on a read (`GET /v1/resources/{id}`).

Two recordings are possibly the same when all of these hold:

- their titles are equal once both are normalised: lower case, `&` as `and`, punctuation as
  space; without the tags that keep a recording's identity, in brackets or after ` - `
  ("Remastered 2011", "2011 Remaster", "Explicit", "Album Version", "Deluxe", "Bonus Track",
  "From the X Soundtrack", a `feat.` or `with` credit); with the tags that name another
  recording ("Live", "Remix", "Radio Edit", "Acoustic", "Instrumental", "Demo", "Extended",
  "Mono", "Stereo", "Part 2", "Taylor's Version"), so "Bohemian Rhapsody - Remastered 2011" is
  "Bohemian Rhapsody" and "Bohemian Rhapsody (Live Aid)" is not;
- their first credited artists are equal, normalised the same way (the artist fact the
  [also-on](#also-on) sources read, else the one the page states; a recording without one
  names nothing);
- their lengths, when both are known (the `duration` fact, else the page's), are within 3 s.

`score` is 1 when both lengths are known and agree, 0.7 when one is unknown. A member of the
card's own group and an alias are never listed, so a pair that a key later joins drops out of
the hint by itself. It is a hint, like a group, and less: nothing is merged, counted or ranked
by it, and each recording keeps its own card.

A background pass computes the pairs, again whenever a recording's name, type, artist or length
changes (its page or the sources' facts); a request only reads them. Each recording keeps its
best twenty pairs and is named by at most forty others, so a change marks a bounded number of
recordings to refill their lists; a read examines at most a hundred candidates, and one that
runs out with room left goes on from the position it stopped at in a later read. An hourly sweep, which also
runs at startup, reads recordings saved before the hint existed or while it was off. Search pages
change their `ETag` when a hint on them changes. `[groups] same_recording_hint = "off"`
([operations](operations.md#groups)) removes the field from recording cards and stops
the pass.

## Possibly the same app

An app's pages on different stores join one group when a Wikidata item names them (the `apps`
lookup family). Without such an item, an app's store card (App Store, Google Play, F-Droid,
Flathub, Snap Store) names the other store cards that are possibly the same app in `maybe_same`:
`{id, uri, site, score}` by URI, up to three on a card and ten in its `details`. Two store cards
are possibly the same app when their official websites have the same host (without `www.`) and
their names are the same once both are in lower case with every run of other characters than
letters and digits read as one space ("Signal - Private Messenger" and "Signal Private
Messenger"). The pair is the `app_match` fact the apps family records for each card (the host and
the name: `signal.org signal private messenger`); `score` is 1. A member of the card's own group,
an alias and a missing card are never listed. Like the event hint, it merges, counts and ranks
nothing, and `[event] same_event_hint = "off"` removes it too.

## Also on

A card's `also_on` says where else the same thing is: first the other members of its
[group](#groups) (links someone tagged, each with its resource `id`), by ID and URI, then
**derived links**: the same thing on services nobody tagged, found by a source, with
`derived: true`, the service's name in `site`, the hub or service it came through in `via`
(`musicbrainz`, `wikidata`), and for some a `kind` its source's page names (`archived`: a Wayback Machine copy, with
its capture time in `at`). Derived links are ordered by service name; each URL appears once (a tagged member wins)
and never the card itself. A card carries at most 8 entries, and one link per service, the
service a link's URL is on (a derived link's `site`): for a music service a tagged member
first, because someone used that link (of several, the one on the recording's original album,
the `original_album` fact), then the derived link most group members name. The card's own
service, by its URL, shows nothing else: an artist's card never shows another page of its
service, such as the band's earlier name, which Wikidata may list as a second ID. A music member
whose card is not read yet counts by the service its URL names, against derived links only: it
keeps derived links on that service off the card, is not deduplicated against other tagged
members (two such members on one service both show until their cards are read), and counts
within the entry cap. Any other tagged member whose URL is on a service a derived link names (a
tagged `f-droid.org` page is on F-Droid, as Wikidata's F-Droid link is) takes that service's
place the same way; a page that only names itself after a service does not, and other tagged
members always show. A card of a service the sources know is named after it (`site`):
Wikidata, OpenStreetMap, DOI and the services each source's page names (the ones its derived
links name). Only an object's own page is on the service, in the forms its source recognizes
(its page says which). Any other page on those hosts (a topic list, a search, a user's page, a
page under an object) takes no service's place; its card keeps the name it gives itself, or its
host. `GET /v1/resources/{id}` lists every link in its `also_on` (several of one service too,
up to 256). A derived link can be unavailable in
some countries (a regional release on a streaming service); nothing public says where, so prefer
a tagged member.

A derived link is a hint: it is never fetched, never counted as a reference and never ranks
anything. Sources read a resource after it is saved and again when their results age (each
source's lifetime: 30 days, `btcmap` 7); a source's links and keys live twice that, so one
failed refresh keeps them, and they go when the source stops returning them. The sources run in this order, each
seeing the keys the ones before it found: `service`, `tidal`, `spotify`,
`musicbrainz`, `musicbrainz_entity`, `performers`, `wikidata`, `discogs`, `deezer`, `itunes`,
`wikipedia`, `wikidata_item`, `books`, `films`, `podcasts`, `product_keys`, `open_food_facts`,
`stack_exchange`, `titles`, `federal_register`, `osm_wikidata`, `wikidata_osm`,
`wikidata_places`, `btcmap`, `unpaywall`, `ncbi`, `datacite`, `huggingface`, `crates`, `npm`,
`pypi`, `software_heritage`, `steam`, `wikidata_stores`, `app_stores`, `wayback`;
`[also_on.sources.<id>] enabled = false` turns one off. A MusicBrainz or Wikidata answer over 8 MB is no answer: the source
records only what the link's URL proves (a MusicBrainz page's MBID or ISRC, a Wikidata item
link's Q-id, the Q-id an article's own Wikipedia names), else nothing, until its next run. A
source's exact keys
(an MBID, a Q-id) also [group](#groups) the links that share them. A derived link found through
an exact ID (a MusicBrainz relation, a Wikidata item's claim or sitelink, a service's answer to
a lookup by barcode or ISRC) carries that ID: a tagged resource that is that link (the same URL
after its aliases) takes it too and joins the group, even when its own lookup finds nothing.
A derived link is stored under its root spelling ([aliases](#aliases): without tracking
parameters, an Apple Music link without its storefront), so one object found through two
spellings is one link, kept by the source that found it first.

**The sources.** Each source's page in [docs/sources](sources/README.md#also-on) says what it reads
and finds and what it costs; the index lists them in the order they run. A music link (a track,
album or artist on a music service, [its spellings](#aliases)) is looked up exactly, never by title
or name. A key comes from a hub (MusicBrainz, Wikidata) or from a known music service's own record
of its own object; another host's claim (its JSON-LD `isrcCode`, `sameAs`, a canonical link) is a
fact at most. ISRCs are stored in upper case and barcodes as GTIN-13 (a 12-digit UPC gains a
leading zero), by every source. The level is exact: a track lists tracks and videos of the
recording, an album lists albums and YouTube's album playlists, an artist lists artist pages and
channels. Album links group by their release group, while two editions (two MusicBrainz releases)
stay apart. Apple Music links carry no storefront; Apple Books and Apple Podcasts links, and the
iTunes lookups, use the storefront `[also_on] apple_storefront` (`us`). MusicBrainz stores an
Apple Music link under the storefront an editor browsed, so a hub lookup of one asks every
common storefront's spelling in one request.

An article's card comes from its Wikipedia's REST summary (`Article`, the title, the first
paragraph up to 600 characters, the thumbnail, `site` `Wikipedia ({lang})`), so a long article
whose page exceeds the page cap still gets one; a page the summary does not cover (a special page)
is read as a web page. A MusicBrainz page's card ([its spellings](#aliases)) comes from
MusicBrainz's web service (the entity it names), not from the page (unless that answer is over its
8 MB cap: then the page is read as any page, and the sources record only what its URL proves, the
MBID or the ISRC): the title, `MusicRecording`, `MusicAlbum`, `MusicGroup` or `Person` (an artist
MusicBrainz calls a person), the artist credit and a recording's length as its description, `site`
`MusicBrainz`; an ISRC's page is its one recording's card, else named `ISRC {code}`.

## Cards and resolvers

Public `/v1/info` names the kinds this instance resolves in `resolves`; the admin `/v1/info`
lists one manifest per resolver and card kind (`media`, `isbn`, `doi`, `nostr`, `activitypub`,
`official`),
the event platform adapters, and each resolver's switch, concurrency, request rate and job
counts by status.

- **ISBN**: ISBN-10/13 checksums, metadata from Open Library.
- **DOI**: `doi:`, `https://doi.org/…` and `https://dx.doi.org/…` request CSL JSON with the
  publication date and container. Invalid bare identifiers are final `missing`; informational
  DOI URLs fall back to web.
- **Nostr**: profiles and notes through the signed-event indexer; NIP-52 events keep their
  event schema; unknown signed kinds give a generic card. Relays (`event.adapters.websocket`)
  are asked only after both HTTP indexers miss.
- **ActivityPub**: advertised endpoints or ActivityStreams JSON give Person, Article or
  SocialMediaPosting cards only when the fetched URL and the object ID share a registrable
  domain (on a multi-tenant host, the same tenant). Note and Article excerpts are capped at 200
  characters; unlisted content is omitted.
- **Media**: oEmbed (`media.oembed`) and page JSON-LD, which wins when healthy, except over a
  provider whose scheme names the link exactly (YouTube's `watch` and `shorts`) or any matching
  provider under `"always"`: their title, author and thumbnail come first. A blank field in an
  answer is no value, so the page's stays. An oEmbed answer survives a failed or truncated page. Podcast types need a namespace or an audio/video
  enclosure; plain RSS is a DataFeed. Direct podcast feeds are PodcastSeries; `?guid=` or the
  episode page's RSS link selects an episode. A feed longer than the page cap is read up to its
  last whole item within the first megabyte (the newest come first), so an episode past that is
  never marked missing. No embed HTML or media bytes are served.
- **Event platforms**: the adapters in `data/event-adapters.json`. Each has an `enabled` switch
  and, where the platform needs one, a `key`; a missing key disables only that adapter and no key
  is ever returned. Its manifest lists the mode, cadence floor, identity, cancellation rule,
  expected fields and canary. BTC Map websites and NIP-52 calendar members are queued as their
  own resources (`event.adapters.derive_resources`).

- **Official APIs**: the [sources](#sources) below.

Disabled identifier routes wait in the queue; an enabled web resolver still gives ordinary HTTP
links a card.

## Lists

A page whose structured data holds several entries of one kind (events, questions, products,
articles, places, media, job postings, recipes) lists them: an artist's concerts, a FAQ's
questions, a category's products, a blog's posts. An entry is an entity naming another page (its
`url`, else `@id`, is another address) or one among several listed as a page's `mainEntity`; an
entity naming no page is the page's own (a product page's variants, a theme's and a plugin's
node for one event), as is one whose `url`, `@id` or `mainEntityOfPage` is the page under any of
its addresses (the one asked for, the one fetched, its head canonical, its `og:url`: an AMP
page's article or event names the canonical), on the web route and the event route alike. The
entries
are parts of the page, never its subject, and are left out before the page's JSON-LD is read,
whichever scripts hold them: the card is the page's own entity (the artist's `MusicGroup` on
its concert list) or else the page (a `WebPage` named by its title), not the first entry. Two
entries with one `url` (else one `@id`, else one name) are one, and on the event route the same
event in several scripts is read as one, whatever `@id` each script gives it: what the first
lacks (a start, a description) comes from the next. On
the event route such a list yields no event, in the page's JSON-LD and in a platform's embedded
state alike. The decision rides on the card (`event.not_event`: `lists_events`, or
`creative_work` for a Wikidata item that is a work): a stored event re-read with it is replaced,
type and all, where a stored event that merely lost its start for one fetch (a consent wall, a
date that failed to parse) keeps its card until the next refresh (drift). A music object's page (a track, an album, an
artist) treats its video (`VideoObject`) as a part too: the card is the track, named by the
page's title, not "… | Music Video".

## Jobs

A page whose JSON-LD is a `JobPosting` (most job boards and career pages publish one) gets a
`JobPosting` card, and so do the postings that [sources](sources/README.md) read from a job
board's or forum's API (their pages say which). The card is named by the posting's title and
described by the start of its text, as plain text.

- `start` is the date posted (`datePosted`; a date alone is its midnight UTC) and `end` the end
  of `validThrough` (a date alone is valid through that whole day, UTC). Search's `from` and `to`
  select by the date posted.
- A posting past its `end` is closed: `facts.job_status` says `closed` (else `open`), and
  search leaves it out unless it sets `closed=true`. Its page is read again a day after it
  closes, then no more.
- The workplace (`jobLocation`) is the card's venue (`venue`), placed on the map as an
  event's venue is ([OpenStreetMap and locations](#openstreetmap-and-locations)): from the
  page's own coordinates; else, with a street, the address geocoded; else the town, when
  Nominatim finds exactly one settlement of that name (and country, when the posting names one),
  in no other region (state, by name or ISO code) and postal region (a numeric postcode's first
  digit) than the posting names. A one-line place ("Springfield, IL", "Austin, TX 78701") is
  checked too: a US state's, Canadian province's or Australian state's code after the town must
  be the settlement's ("London, ON" is not London, England); any code that is the settlement's
  own ISO 3166-2 code agrees ("Chennai, TN" is Chennai, IN-TN), and a numeric postcode there (four
  digits or more, or three and two: "114 55") its postal region. Other words after a town ("UK",
  "EU", "HQ", "District 1", a region in full) are left to Nominatim, which reads the whole line. The card's `point` is then that place's,
  with OpenStreetMap's attribution.
- A posting is remote when its `jobLocationType` is `TELECOMMUTE` or its place says "Remote"
  ("Remote - US"); it then names the regions it accepts applicants from
  (`applicantLocationRequirements`, or those its place names after "Remote"), up to 32. A place
  is read from its first 1,024 bytes; a region the cut falls in is left out. A posting can be
  remote and have a workplace.

| Fact | Meaning |
| --- | --- |
| `hiring_organization`, `hiring_organization_url` | The employer and its own site |
| `employment_type` | `FULL_TIME`, `PART_TIME`, `CONTRACTOR`, `TEMPORARY`, `INTERN`, `VOLUNTEER`, `PER_DIEM`, `OTHER` (or the posting's own word in that form); several joined with `; ` |
| `remote`, `remote_regions` | `true`, and the regions it accepts applicants from, joined with `; ` |
| `salary_min`, `salary_max`, `salary_currency`, `salary_unit` | The base salary: a range or one amount in both, its ISO 4217 currency, per `HOUR`, `DAY`, `WEEK`, `MONTH` or `YEAR` |
| `date_posted`, `valid_through` | As the posting gives them |
| `apply_url` | Where to apply: its ApplyAction, else its own `url`, else the page when one applies there (`directApply`) |
| `job_status` | `open` or `closed`, at the time of the read |

## Products

A page whose JSON-LD is a `Product` (or a subtype: `ProductGroup`, `IndividualProduct`, …) is a
product card, and so are the products [sources](sources/README.md) read from a shop's own product
record or a product database (their pages say which).

- Its barcode, `facts.gtin`, is the product's own GTIN as GTIN-13: from its `gtin13`,
  `gtin`, `gtin12`, `gtin14`, `gtin8` or `isbn` (a book's), or its one offer's, with the check
  digit verified. The barcode, `sku` and `mpn` come only from the page's own product (in the
  page's head, or naming the page; a page served as JSON-LD is all head, under the same rules), never from another product a page lists; a page with
  several products of its own (a category page's head list) states the barcode of none. One
  product described twice (by the same `@id`, else `url`) is one product; two `@id`s are two
  products, whatever `url` they share. A product naming the page only by a fragment (its `url`, else
  its `@id`, is `#nutella` or the page's URL with a fragment, in each of its descriptions) is
  one the page lists when the page's head
  declares the page to be a `CollectionPage` or `SearchResultsPage`, or names another main
  entity; a product page's `#product`, which its head names as its `mainEntity` or declares
  nothing about, is its own. Among products otherwise alike, one naming no page (the page's
  own, in its head) is the card over one naming another page, and so is a script whose card
  would be the page's own over one whose card names another page (in a `@graph`, an array or
  a page's `mainEntity`, beside page metadata such as a `WebPage` or breadcrumbs or not). A
  `ProductGroup`
  names the barcode, article numbers and offers of the variant its address picks (its `url`, or
  `?variant=`), else of its only variant, never the group's. A product's barcode is the key `gtin:`, so
  the same product on several shops is [one group](#groups), and `key=gtin:{barcode}` finds them
  all.
- Its price and availability are facts with the time they were read, never identity: the cheapest
  offer's `price`, `price_currency`, `availability` and `seller` (all four of that one offer),
  and `price_at`.
- Open product databases ([sources](sources/README.md)) answer for a barcode with a derived link
  and their facts, under the ODbL: show `facts.attribution` (or the card's
  `attribution`) with them.
- Amazon pages are never fetched, on any of its shops (`amazon.com`, `amazon.ie`,
  `amazon.co.za`, …) or short links (`amzn.to`, `a.co`, …), also where another link redirects
  to one (refused, class `policy`): an Amazon product link (`/dp/{ASIN}`, `/gp/product/{ASIN}`) is a
  `Product` from the link alone, with its `asin` (and `isbn`, for a book's ISBN-10), and any other
  Amazon link a `WebPage`.

| Fact | Meaning |
| --- | --- |
| `gtin` | GTIN-13 |
| `brand`, `sku`, `mpn` | The brand, the shop's article number, the maker's part number |
| `price`, `price_currency`, `availability`, `seller`, `price_at` | The cheapest offer's price, its ISO 4217 currency, its availability (`in_stock`, `out_of_stock`, `pre_order`, `back_order`, `discontinued`, `limited_availability`, `sold_out`, `in_store_only`, `online_only`, `reserved`, `made_to_order`), who sells it, and when that was read (RFC 3339) |
| `asin` | An Amazon link's ASIN |

## Questions

A page whose JSON-LD is a `Question` (a `QAPage`'s single main entity, as Stack Exchange's), and
the questions [sources](sources/README.md) read from a Q&A site's API or a forum topic marked
solved, are `Question` cards. A `FAQPage`'s questions (its main entity, one or many) are parts of
the page, never the page itself: its card stays the page's (`FAQPage`, a `WebPage`), named by the
page's own title, as a homepage with a FAQ block is. `Question` cards are named by the question's title, described by the start of its text (200
characters, as a quoted post is), with its asker as `author` and when it was asked as
`published`.

| Fact | Meaning |
| --- | --- |
| `answer_count`, `score` | Its answers and votes, as its site counts them |
| `accepted_answer` | The address of the answer its asker accepted |
| `closed_reason`, `duplicate_of` | Why it was closed, and the question it repeats (a link; several joined with `; `) |

A question closed as a duplicate of one other question is in that question's group (a hint, as
every group is), where its site's source says so.

## Recipes

A page whose JSON-LD is a `Recipe` is a recipe card; on a recipe page the recipe outranks the
article or video that wraps it. Only the page's own recipe does (it names the page, or names
none): a recipe of another page that an article recommends never displaces the article,
whichever comes first and whether or not the article names its URL.

| Fact | Meaning |
| --- | --- |
| `total_minutes` | From `totalTime`, else `prepTime` plus `cookTime` |
| `recipe_yield` | As the recipe gives it (`4`, `6 servings`) |
| `ingredients` | Each `recipeIngredient` without its amount, unit (a unit word that ends the line is the ingredient: `2 cloves`), notes in parentheses, preparation after a comma (`beaten`, `chopped`, …) and alternatives (`plus …`, `or …`), lower case, joined with `; `, as many whole ones as 256 bytes hold |
| `cuisine`, `recipe_category` | As the recipe names them, joined with `; ` |
| `diet` | `gluten_free`, `vegan`, `vegetarian`, … (schema.org's `RestrictedDiet` in snake case) |
| `rating`, `rating_count` | Its aggregate rating |

Search by ingredient is `ingredient=` over that list, by whole words, not a full-text search:
`ingredient=chickpeas,lemon` lists the recipes that have both.

## Sources

Where a service's page is walled, thin or moved, Pinakes reads the service's own API for its
links instead (keyless, except GitHub, which needs `[sources.github] token` or
`PINAKES_GITHUB_TOKEN` and is off without one): the read's `from` names the source and `facts` holds the
API's fields. [docs/sources](sources/README.md) lists every source with the links it reads,
what it fetches, its card, its facts, its limits and licence, and its examples; each can be
switched off in `[sources.<name>]`.

## OpenStreetMap and locations

OSM object URLs (`www.openstreetmap.org`, `openstreetmap.org`, `osm.org`, `www.osm.org`) resolve
through the OSM resolver, including object aliases: `/{type}/{id}/history[/{version}]`,
`/edit?{type}={id}`, object shortlinks (`/go/<code>?{type}={id}`), the editing API
(`api.openstreetmap.org/api/0.6/{type}/{id}[/full][.json|.xml]`) and Nominatim
`details`/`ui/details.html?osmtype=N|W|R&osmid=` (exactly one `osmtype` and `osmid`, no
`place_id` or `postcode`). Every spelling is an [alias](#aliases) of the canonical object
URL, so one resource and one cached `osm_place` row serve them all. OSM cards carry `attribution`; details
add the category, address and bbox. Any other URL on an OSM host (user pages, map views,
changesets, notes) is refused before any fetch: status `error`, a link-only card, stored once
as final. Batch 429, 5xx, timeouts and oversized bodies defer jobs without charging an attempt,
honouring a Retry-After of up to one hour.

`geo:` URIs (RFC 5870, altitude and uncertainty ignored; other CRSs are errors) and location
links resolve offline as Place points (`geo.enabled`): OSM marker links (`/?mlat=&mlon=`), OSM
shortlinks without an object, OsmAnd `osmand.net/map?pin=` and `osmand.net/go?lat=&lon=`, and
Organic Maps `omaps.app/<ge0 code>`. Points are not identities and never group. Pinakes does no
reverse geocoding and offers no place search; event venues are geocoded through Nominatim
(`event.geocoding`).

`GET /v1/dumps/osm_place.ndjson.gz` is the ODbL derivative database as a static file the daemon
writes every `limits.osm_dump_every_hours` (24) from one repeatable-read snapshot
(`application/gzip`, `Content-Disposition: attachment`). `/v1/info` gives its path and
`updated_at` in `osm_licence`. Decompressed, its first line is the licence notice (`notice`,
`licence`, `licence_url`, `attribution_url`, `table`), then one object per cached OSM object:

| Field | Meaning |
| --- | --- |
| `osm_type`, `osm_id` | The OSM object (`node`, `way` or `relation`, positive id) |
| `name`, `type`, `type_path`, `category` | Current name, schema.org type and its path, OSM `key/value` category |
| `point`, `bbox` | `{lat, lon}` and `[min_lon, min_lat, max_lon, max_lat]`; null for missing objects |
| `address`, `tags`, `display_name` | Address parts, allowlisted OSM tags, Nominatim display name |
| `status` | `ok`, or `missing` for cached missing records |
| `fetched_at` | When Nominatim last returned the object |
| `versions` | Every derived document served publicly, keyed by version, so earlier values behind the change journal are in the dump |

The raw fetch, the search vector and the refresh schedule are not exported; a new column is not
exported until it is added here, to the OpenAPI schema `OsmDumpPlace` and to the dump query. No
Pubky references or scores are exported.

A download reads only the file, never the database. It carries `ETag`, `Last-Modified` and
`Cache-Control: public, max-age=3600`; `If-None-Match` or `If-Modified-Since` answer 304, which
costs nothing. One byte range (`Range: bytes=…`, with `If-Range`) answers 206, an unsatisfiable
one 416. Only a transfer (200 or 206) counts against `limits.osm_dump_downloads_per_hour` (6)
per IP (429 beyond it); `HEAD`, 304 and 416 are free. Before the first file is written the route answers 503 with `Retry-After`. A
new file replaces the old one atomically, so a download in progress finishes the file it began.
Coordinate-derived time zones use ODbL boundary data too; see `data/README.md`.

## Limits and errors

Every public route (`/v1`, Swagger UI, the agents' guide and index, and `/mcp`) shares a per-IP budget on the direct peer address
(`api.requests_per_minute`, 120; forwarded headers are not trusted). A search costs
`limits.reach_cost` (6) units for each of `reach` and `boost` (each reads a set from Nexus),
charged before any Nexus call, `limits.related_cost` (6) for `related`, all of them together
when it has several; every other request costs one.
Over budget returns 429 with `Retry-After: 60`. The public and admin listeners share a
ten-connection pool, apart from the workers', with a five-second statement and acquisition
timeout. The dump is written by the daemon,
not by a request: its writer's read-only transaction lifts the statement timeout to drain a large
snapshot, and the API only serves the file.

Handler errors are `{"error": "…"}`; Axum's query and JSON parse errors may be text. Malformed
query parameters and JSON return 400, JSON of the wrong shape 422, bodies over 2 MB 413.
Internal errors return 500 without database details, and a handler panic is answered 500 by a
backstop layer (logged with its path, counted in `pinakes_api_panics_total`), never a dropped
connection. There are no resource write, social-state, reverse-geocoding or place-search
routes.

## Agents

AI agents read Pinakes as nexus-scout serves the social graph: a usage guide, and an MCP endpoint.
Both are on the public listener under the same per-IP budget, and neither needs a key.

**The guide.** `GET /llms.txt` (plain text) explains what Pinakes indexes, then a quickstart,
recipes for the questions agents ask (what is this link; places near a point with a tag; events in
a city this week; every link with one ISBN, DOI, ISRC or OSM id; what else is the same thing),
how to join with Nexus by URI, the MCP tools, the limits and what to do on each error. It names
nexus-scout for graph questions. `GET /skill.md` is the same guide with skill front matter
(`name: pinakes`). The base URL in both is the host that served them: the request's `Host`, with
`X-Forwarded-Proto` when a proxy names the scheme, else `http` for a loopback host and `https`
for any other. `GET /` links the guide, the skill, the MCP endpoint (when on), the OpenAPI document,
`/v1/info` and nexus-scout's guide.

**MCP.** `POST /mcp` (`api.mcp`, default on) is a Model Context Protocol server over Streamable
HTTP, stateless and read-only, built on the official Rust SDK (`rmcp`). Messages are answered as
JSON (`application/json`); there are no sessions and no server-to-client requests. Each tool is a
request to the public routes above, answered by their handlers under the public listener's
layers, with the caller's address:

| Tool | Request | Answer |
| --- | --- | --- |
| `search` | `GET /v1/search` with `q`, `type` (array), `tags` (array), `match`, `near` (`{lat, lon, radius}`), `bbox` (`[west, south, east, north]`), `from`, `to`, `key`, `sort`, `window`, `collapse`, `reach`, `boost`, `viewer`, `limit`, `cursor` | Compact cards, `cursor`, `facets` |
| `card` | `GET /v1/resources/by-uri`; with `resolve: true` and an unknown link, `POST /v1/resolve` | One compact card with `category`, `address`, `facts`, `aliases` (at most 10) and the event fields |
| `lookup` | `GET /v1/search?key=` | Compact cards |
| `group` | `GET /v1/groups/{id}` | `id`, `type`, `keys`, every member's compact card, and `best`: the most complete readable member of the group's type |
| `types` | `GET /v1/search?facet=type&limit=1` and `GET /v1/info` | The card types with their counts, and `resolves` |
| `social` | `POST /v1/resources/lookup` for card `ids`, then Nexus `POST /v0/stream/resources/by_ids` (100 URIs a call) with each card's `uri` and `refs`, and any plain `uris`; 1–100 entries together, optional `viewer` | Per card (or link): `asked` (the URIs sent, the card's own first), `found` (those Nexus knew), and the tags merged over them: per label `taggers_count` (the taggers the views list, each once, or the most one view counts, whichever is more) and `viewer_tagged`; `taggers_count` over all labels, the same way. A view counts taggers it does not list, who may be ones another view lists: over several such views a count is a lower bound, marked `at_least: true`. An alias's id (a spelling, or a card that became one after it was read) is answered under the id asked, with `root`, the card that answers for it. `unknown_ids` names ids Pinakes does not know |

A compact card keeps `id`, `uri`, `status`, `type`, `name`, `description` (at most 300
characters), `image`, `site`, `point`, `start`, `end`, `group`, `also_on` (`uri`, `site`), `refs`,
`occurrence`, `event` and `attribution`, and adds `url`, the card's full read
(`GET /v1/resources/{id}`); every answer names the REST URL of its full result. A failed request
is the tool's error (`isError`), its text `{error, status, hint, url}` with the route's status
and message; an unknown tool is a JSON-RPC error.

**A browser on `/mcp`.** A `GET /mcp` that accepts `text/html` answers a short HTML page: what
the endpoint is, the Claude Code line (`claude mcp add --transport http pinakes {base}/mcp`),
the JSON configuration other clients take, the tools' names and links to `/llms.txt` and
`/skill.md`, with the base as the guide's. Any other GET is the 405 the protocol allows, with
`Allow: POST` and one line of plain text; every other method is MCP's.

**Budget.** An MCP message spends one unit of the per-IP budget, and a tool call what its
requests cost on top (a `reach` search 6, `types` two requests, `social` its lookup plus one per 100 URIs it asks Nexus). Bodies are capped
at 2 MB, as for every route; the routes keep their statement timeout, and `social` the Nexus
client's 10-second timeout. When Nexus does not answer the batch view, `social` is the error
`502`.

**No admin operation.** The tools' requests go to the public routes only, built afresh with no
header of the MCP request, so the admin listener's routes and its token are out of reach. Any
`Host` is accepted: the endpoint takes no credentials and changes nothing, so rmcp's DNS-rebinding
guard for local servers protects nothing here, and the public host names are the reverse proxy's
to know.

## Admin listener

`admin_listen` (default `127.0.0.1:9090`) serves `GET /metrics` (Prometheus text),
`GET /v1/info` with full ingestion, source and canary state, `GET /v1/resources/{id}/document`
(the stored document with its internal fields) and group curation (`PUT /v1/groups/{id}`). With
`admin_token` (or `PINAKES__ADMIN_TOKEN`) every admin route needs `Authorization: Bearer`,
compared in constant time; a non-loopback bind without a token is refused. It sends no CORS
headers. Public `/v1/info` shows only what a client needs (the version, the User-Agent this
instance's requests carry, the kinds of links it resolves, the lookup sizes, the search page
limit, the per-IP and preview budgets and the OSM licence); the admin `/v1/info` adds manifests,
resolver status, counts, ingestion,
sources and metrics. Errors are stored only as sanitized classes. Admin info and database
metrics are cached for 15 seconds. Metrics and alerts are described in [ops](../ops/README.md).

Canaries (`canary.enabled`) probe each enabled adapter every `limits.canary_interval_secs`
(6 hours) under the ordinary worker and host budgets. A web canary denied by host policy is
skipped; OSM probes bypass the cache without writing it. A gauge appears only after the first
run.
