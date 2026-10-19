# Pinakes API v1

Pinakes returns link metadata (cards), search and ranking over the links people reference on
Pubky, calendar dates, groups of links to the same thing, and the changes to the links a client
keeps. Clients hydrate social state (posts, tags, users) from Nexus in a separate call, by URI:
a card's `uri` (and `details.aliases`), never its `id`, which is Pinakes's own. Paths are
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
| `GET /v1/resources/{id}` | 32-character resource ID | Card plus `details`; 404 if unknown |
| `GET /v1/resources/by-uri` | `uri`: any spelling of a link | As `GET /v1/resources/{id}` for the URI's ID (an alias answers with its root); 404 if unknown, nothing is fetched |
| `POST /v1/resources/lookup` | Exactly one of `{"ids":[…]}` (alias `resource_ids`) or `{"uris":[…]}`, 1–500 entries | `[card]` in request order, duplicates kept, unknowns skipped |
| `POST /v1/resolve` | `{"uri":"https://example.org/"}` | Card plus `details` now or within 3 seconds; 202 with `status: "pending"` while queued |
| `GET /v1/resources/{id}/occurrences` | Optional RFC 3339 `from`, `to`, `limit` (1–500, default 100), signed `cursor` | `{items: [occurrence card], cursor}` |
| `POST /v1/occurrences/lookup` | `{resource_ids?, uris?, from?, to?, limit?, cursor?}`: 1–500 feed IDs and links together | `{items: [occurrence card], cursor}` across the feeds, by start |
| `GET /v1/groups/{id}` | 32-character group ID | `{id, type, curated, keys, members: [card]}`; 404 if unknown |
| `POST /v1/changes/lookup` | `{resource_ids?, uris?, since?, cursor?, limit?}`: 1–500 IDs and links together | Change page for those resources |
| `GET /v1/info` | None | `{version, resolves, limits, osm_licence}`: what this instance resolves and the limits a client must respect |
| `GET /v1/dumps/osm_place.ndjson.gz` | None | The ODbL derivative database as a daily gzip file ([OSM](#openstreetmap-and-locations)) |
| `GET /v1/openapi.json` | None | OpenAPI 3.1 |
| `GET /swagger-ui` | None | Swagger UI (when `api.swagger_ui`) |

URIs are normalized and IDs computed by Pinakes itself: lowercase scheme and host, no default
port, userinfo or fragment, then BLAKE3. Each URI is at most 2048 bytes; IDs are 32 lowercase hex
characters and Pinakes's own, so combine with other indexes by URI. A missing upstream object is a known resource with
`status: "missing"`, not a 404. `lookup` never fetches an unknown URI; a known overdue resource
moves to the front of the refresh queue without bypassing its TTL, backoff or final state.

## Response shapes

Every route returns the same card for a resource, an occurrence or a Pubky event reference.
Absent values are `null`, never `""`. Text fields (`name`, `description`, `site`, and every
text in `details`) are plain text, decoded from the source's HTML entities once: escape them
before rendering them as HTML.

A music link is typed by what it is, whatever the service's page or player calls it: a track is a
`MusicRecording`, an album a `MusicAlbum`, an artist a `MusicGroup` (a `Person` from the next
refresh after MusicBrainz says the artist is one) and a playlist a `MusicPlaylist`, on every
service ([music service links](#aliases)). A video stays a `VideoObject`. The card's `site` is the
service's name (`Spotify`, `Apple Music`, `TIDAL`, `YouTube`, …), as derived links show it, in
any page language. A Bandcamp root or `/music` page features an album but is the artist's page: a
`MusicGroup` named after the artist, without the album's description. When the service's own
page names the artist (JSON-LD `byArtist`, as TIDAL's do), the description says what the page
does instead of its share text: a track's `Queen · A Night At The Opera (Deluxe Edition) · 5:55`
(artist, album, length), an album's `Tame Impala & JENNIE · 2025` (artist, year); the
`service` source adds `artist`, `album` and `duration` (seconds) to `details.facts`.

### Card

| Field | Meaning |
| --- | --- |
| `id` | Resource ID; for an occurrence, the feed's ID |
| `uri` | The resource URI; for an occurrence, its own URL when it has one, else the feed's. To combine with Nexus, send `uri`, and for an occurrence `occurrence.feed_uri` (its own URL was never tagged) |
| `status` | Fetch health: `ok`, `pending`, `missing`, `unreachable` or `error` |
| `type` | schema.org type (`MusicEvent`, `Book`, `Place`, …), or one of Pinakes's own types for what schema.org has no class for: `BitcoinBlock`, `BitcoinTransaction`, `BitcoinAddress`, `LightningNode` and `LightningChannel` (not schema.org; each a child of `Thing`) |
| `name`, `description`, `image` | Display metadata; images point to the origin and are never proxied |
| `site` | The publishing host; for `pubky://` objects the app path segment (`mapky.app`), not the public key; for a card read from an official API without a host of its own, the service's name (`Federal Register`) |
| `point` | `{lat, lon}` |
| `start`, `end` | RFC 3339 instants for events and occurrences |
| `group` | `{id, size}` when the resource or occurrence is in a [group](#groups), else `null` |
| `also_on` | The same thing elsewhere ([Also on](#also-on)): the other members of its group as `{id, uri, site}` (other resources first, one entry each by ID, then other occurrences of its own feed), then derived links nobody tagged as `{uri, site, derived: true, via, kind?, at?}`, by service name; one per music service, at most 8 on a card and 256 on `GET /v1/resources/{id}`, whose `details.also_on` lists every link. A hint only: every member keeps its own card |
| `attribution` | Present on OSM cards, events placed at an OSM venue and events whose zone came from coordinates; display it with the card |
| `occurrence` | Occurrence cards only: `{feed_id, feed_uri, uid, recurrence_id, status}`; `feed_uri` is the feed's URI, the link people tagged |
| `event` | Pubky event references only: `{author, object_path, recurrence_id, start, end, status}` |
| `maybe_same` | Event and occurrence cards only: up to three other events that are [possibly the same event](#possibly-the-same-event), best first, each `{id, uri, site, start, score}` (`[]` when there are none). A hint only, like `group`. Absent on other cards, and on every card with `[event] same_event_hint = "off"` |

A `pubky://` object without a name, title or description takes its description from a
content-like member (`content`, `text`): such a card is a `SocialMediaPosting`, social content like a note, with a 200-character excerpt that refreshes within the hour and is removed once the object is gone.

### Resource detail

`GET /v1/resources/{id}`, `GET /v1/resources/by-uri` and `POST /v1/resolve` return the card's
fields plus `details`. The two reads carry `Cache-Control: public, max-age=30` and an `ETag` of the
body, and answer `If-None-Match` with 304.

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
| `facts` | The fields that API names for the object, by name (`document_type`, `citation`, …), then the facts the [also-on](#also-on) sources read for it (`isrc`, `upc`, `mbid`, `artist`, `artist_type`, `duration` in seconds); several values of one name are joined with `; `. Each source's page lists its facts. `null` when there are none. Facts never join anything |
| `also_on` | Every link of the card's [also on](#also-on), several of one music service too (the card shows one per music service) |
| `aliases` | Other spellings of this link that resolve to it ([aliases](#aliases)), at most 50; a page's own `url`, `@id`, `sameAs` or canonical claims are not aliases and are not listed |
| `updated_at` | When the current metadata was accepted |
| `maybe_same` | An event's card only: up to ten events that are [possibly the same event](#possibly-the-same-event), best first (the card's `maybe_same` is the first three) |

Internal fields (the resolver, TTL, group keys, raw attributes, parser warnings) are not public;
operators read the stored document with `GET /v1/resources/{id}/document` on the admin listener.

### Occurrence

An occurrence card is a card with `occurrence: {feed_id, uid, recurrence_id, status}`. A null
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

`{seq, resource_id, occurrence?, at, class, fields}`: `at` is Unix milliseconds, `occurrence`
the serialized `[uid, recurrence_id]` key of an occurrence change, and `fields` every changed
field. Read the current card for the new values.

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
of the same URIs and checks that this list matches.

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
| `POST /v1/resolve {"uri":"https://infomaniak.events/en-ch/conferences/seminaire-des-autorites-2026/92392f2a-6711-4917-b9e9-60c88e8c550a/events/396911"}` | Event |
| `POST /v1/resolve {"uri":"https://xceed.me/en/roma/event/dissonanze-2026-day-1/220015"}` | Event |
| `POST /v1/resolve {"uri":"https://www.ents24.com/london-events/eventim-apollo/simon-brodkin/7474018"}` | Event |
| `POST /v1/resolve {"uri":"https://www.ticketino.com/en/Event/TICKETINO-BBQ/192364"}` | Event |
| `POST /v1/resolve {"uri":"https://api.btcmap.org/v4/events/1"}` | Event |
| `POST /v1/resolve {"uri":"nostr:naddr1qqjrvvp3vycxye33956nycec956x2ery94snqvfk95ergefnx9snqdeh8yekyq3qxfx8p5gzdyv0kgakc365pag8ej2p6g7l72k0hszt0g5m0k0wyymsxpqqqp7txuf3y56"}` | Event |
| `POST /v1/resolve {"uri":"pubky://51da3n5m8s6oaq38uqs7jznp6ezbc3qbtmic8oy6fj3g6mokdyco/pub/eventky.app/events/0034FED2YCDKG"}` | Event |
| `POST /v1/resolve {"uri":"nostr:naddr1qqjx2dryvcmn2c3395enwce4956rxeny943x2c35943kvetrvf3xycmyvvcrjq3qs0mv35668yg7t4jk49zdltkezn0em0s3jpt2qn8mkneurnw2edwsxpqqqp7tgmysyuc"}` | Collection |
| `POST /v1/resolve {"uri":"https://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"webcal://calendar.google.com/calendar/ical/en.ch%23holiday%40group.v.calendar.google.com/public/basic.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://www.calendarlabs.com/ical-calendar/ics/76/US_Holidays.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://ti.to/scipy/scipy2026.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://pretix.eu/demo/democon/ical/"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://api.weezevent.com/sites/view/live/E1114506.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://eventfrog.ch/stream/de/event/7503702322012243890.ics"}` | DataFeed |
| `POST /v1/resolve {"uri":"https://s3-eu-west-1.amazonaws.com/starticket/wwwroot/ticketing/feeds/starticketfeed_D.xml"}` | DataFeed |
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
| `POST /v1/resolve {"uri":"https://n26.com/"}` | WebPage |
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
| `POST /v1/resolve {"uri":"pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G"}` | SocialMediaPosting |
| `POST /v1/resolve {"uri":"pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/reviews/00355XPM88EHG"}` | SocialMediaPosting |
| `POST /v1/resolve {"uri":"pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/collections/00351MVGE211G"}` | CreativeWork |
| `POST /v1/resolve {"uri":"pubky://9g3zsjpwijn3gsh7ibghjxxadpjidceos5rw4387a6hhziqi8aoy/pub/mapky.app/geo_captures/0035C4V0GVX90"}` | CreativeWork |
| `POST /v1/resolve {"uri":"pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json"}` | CreativeWork |
| `POST /v1/resolve {"uri":"pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"}` | CreativeWork |
| `POST /v1/resolve {"uri":"https://www.mexc.com/"}` | `unreachable` |
| `POST /v1/resolve {"uri":"pubky://dir8gnjfnhdn5cpo8gxmfxfpqm9kffkoqpmsefcjkdpkxo15d5ey/pub/pinakes.e2e/none.json"}` | `missing` |
| `POST /v1/resolve {"uri":"https://www.privacyguides.org/dns/"}` | `pending` |

### POST /v1/resources/lookup: several cards

```text
POST /v1/resources/lookup {"uris":["https://luma.com/376whqyw","https://www.meetup.com/bitcoin-meetup-switzerland/events/316899925/","nostr:naddr1qqjrvvp3vycxye33956nycec956x2ery94snqvfk95ergefnx9snqdeh8yekyq3qxfx8p5gzdyv0kgakc365pag8ej2p6g7l72k0hszt0g5m0k0wyymsxpqqqp7txuf3y56"]}
POST /v1/resources/lookup {"uris":["https://www.openstreetmap.org/node/240109189","geo:47.3769,8.5417"]}
POST /v1/resources/lookup {"uris":["pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G","pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json","pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"]}
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
GET /v1/search?key=mbid:9f9cf187-d6f9-437f-9d98-d59cdbd52757
GET /v1/search?key=isrc:GBAYE9701376
GET /v1/search?key=upc:634904078164
GET /v1/search?key=wikidata:Q9312
GET /v1/search?collapse=group&type=Book
GET /v1/search?collapse=group&type=ScholarlyArticle
GET /v1/search?bbox=8.90,45.98,9.00,46.03&cluster=12
GET /v1/search?bbox=8.45,47.33,8.62,47.42&cluster=10
GET /v1/search?bbox=13.25,52.45,13.55,52.58&cluster=14
GET /v1/search?reach=following&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
GET /v1/search?reach=friends&viewer=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
```

### GET /v1/resources/{id}

```text
GET /v1/resources/81cefcae5a6a468cfc27ff1c1b420fce
GET /v1/resources/7f8f0e5f5b301b55a06d51df5d54281c
GET /v1/resources/2dea92aa8c3cf8ae1cffad92cf3ca718
GET /v1/resources/5bccc93f427b6fd44115fdd02618392f
GET /v1/resources/0f3820de1c3532e8805674cc996978f9
GET /v1/resources/fe358c60c13f6a97b50899c4240f27bf
GET /v1/resources/586d741bd42f9a0dde1c946b58db20ae
```

### GET /v1/resources/by-uri

```text
GET /v1/resources/by-uri?uri=https:%2F%2Fwww.openstreetmap.org%2Fnode%2F240109189
GET /v1/resources/by-uri?uri=https:%2F%2Fdoi.org%2F10.1038%2Fnature12373
GET /v1/resources/by-uri?uri=https:%2F%2Fluma.com%2F376whqyw
GET /v1/resources/by-uri?uri=isbn:9780140328721
GET /v1/resources/by-uri?uri=pubky:%2F%2Fc5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey%2Fpub%2Fmapky.app%2Fposts%2F00351MT54YS9G
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
POST /v1/changes/lookup {"since":"now-30d","uris":["pubky://c5nr657md9g8mut1xhjgf9h3cxaio3et9xyupo4fsgi5f7etocey/pub/mapky.app/posts/00351MT54YS9G","pubky://3jubjyq4fkh4dq38exrpuo8we6xta8a6rhxnjjzyoo7j4r3f4rjo/pub/loopky/decks/medeng2026a/manifest.json","pubky://47ccxgzt77jgnffqn1rp7neba3os3py3649cz9e9or3fy76ohqao/pub/pubky-rooms/rooms/0035SFYCGVBV2"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["https://www.youtube.com/watch?v=dQw4w9WgXcQ","https://vimeo.com/76979871","https://www.youtube.com/watch?v=Up2i2Z4m-Kg"]}
POST /v1/changes/lookup {"since":"now-30d","uris":["isbn:9780140328721","doi:10.1038/nature12373","https://doi.org/10.1038/nature12373"]}
POST /v1/changes/lookup {"resource_ids":["81cefcae5a6a468cfc27ff1c1b420fce","5bccc93f427b6fd44115fdd02618392f"],"since":"now-7d"}
```

## Search

| Parameter | Meaning |
| --- | --- |
| `q` | Full-text name/description search with a trigram/partial-name fallback; at most 256 bytes. A post whose author opted out of search indexing (`details.facts.opted_out`: Mastodon's `indexable: false` or `noindex`, Bluesky's `!no-unauthenticated`) never matches |
| `type` | A schema.org type, ancestors included (`Event` matches `MusicEvent`; `Place` matches cafés and other `LocalBusiness` types) |
| `category`, `host` | Exact filters |
| `key` | Links carrying one exact identifier, on any service and in any spelling: `mbid:{uuid}` (a MusicBrainz recording, release, release group or artist), `wikidata:{Q-id}`, `isrc:{code}` (any letter case, hyphens allowed) or `upc:{barcode}` (a UPC, EAN or GTIN in any of its lengths, check digit verified), as the [also-on](#also-on) sources found them as keys or facts, or as an exact lookup by that key named the link; anything else is 400 |
| `tags` | Comma-separated labels; at most 5, each 1–128 bytes |
| `match` | `any` (default) or `all` (at most 3 tags) |
| `author` | Resources referenced by this 52-character z-base-32 public key |
| `reach`, `viewer` | `following` or `friends` of the Nexus user `viewer`: only their references and tags count |
| `venue` | Events at this venue resource ID |
| `sort` | `activity` (default), `recent`, `distance` (requires `near`), `start` or `trending` (by the references made inside `window`; only links referenced in it; with `collapse=group` one song across services is one row, ranked by the references all its links got in the window) |
| `window` | With `sort=trending`: `{n}h` or `{n}d`, from `1h` to `90d`, default `7d`, counted back from the first page's instant |
| `limit` | 1–500, default 50 |
| `cursor` | The signed next-page token of the previous page |
| `facet` | `tags`, `type` or both (`type,tags`): counts over the whole filtered set, in `facets.tags` (`{label, count}` per label) and `facets.type` (`{type, count}` per card type), most frequent first |
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
start. A Pubky event result carries `event` for Nexus post hydration and no name: Pinakes does
not fetch the post. Author and reach filters also constrain that post's author.

**Ranking.** `activity` combines distinct taggers and references with a 30-day decay:

```text
(log(1 + taggers) + 0.5*log(1 + replies) + 0.3*log(1 + embeds + collections))
  * 2^(-age_of_newest_reference_in_days / 30)
```

`recent` ranks by the newest reference; `distance` and `start` by negative metres and negative
Unix seconds, so nearest and earliest come first. Scores are internal and never returned.

**Paging.** Pages are keyset-ordered on `(rank, id)`. The cursor is signed with a key created at
daemon startup (HMAC-BLAKE3) and binds the filters, sort and decay instant; keep them unchanged
(`limit` may change). A tampered cursor, a cursor for other filters or one from a previous
daemon returns 400: restart without a cursor. A null cursor ends the results. Concurrent
updates are visible between pages; this is not a frozen snapshot.

**Map clusters.** `cluster=<zoom>` returns `{resolution, clusters, points, truncated, total,
reach_truncated, facets?}`. A cluster is `{cell, count, lat, lon, top, attribution?}`, where
`top` holds at most three card IDs by rank and the centroid is the circular mean of member
points. Points are cards. The H3 resolution comes from `limits.h3_resolutions` by floored zoom;
large viewports coarsen to stay within `limits.max_cluster_cells` and `resolution` reports the
value used. Singleton cells are points, and when the filtered total fits in `limit` every unit
is a point. `limit` caps clusters plus points; `truncated` marks omitted units, and otherwise
the cluster counts plus the points equal `total`. Cluster mode has no cursor: refine the
viewport.

**Groups.** Without `collapse`, every member of a group is a row of its own, ranked on its own
references. `collapse=group` filters first, then keeps one row per group: the matching member
with the most references (ties by the lower ID), as its own card, ranked as itself. Facets count
every matching link either way; clusters always count links, so `collapse` is refused with
`cluster`.

**Reach.** Following and friends lists come from Nexus in pages of 200, at most 1,000 authors,
cached per viewer for five minutes. `reach_truncated: true` warns that the list was capped. All
pages share `limits.reach_timeout_secs` (5 s); an incomplete list is not cached. An unknown
Nexus user has an empty reach; a Nexus error or the deadline returns 502.

**Caching.** Responses carry `Cache-Control: public, max-age=30` and an `ETag` over the
parameters, the ingestion cursors and epochs, a write revision and the reach set's hash and
truncation; `If-None-Match` returns 304 when nothing changed.

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
- `pending`: queued and not yet resolved.
- `missing`: confirmed gone by two fresh fetches; one missing answer keeps the last accepted
  metadata, and a repeated cached observation does not count twice.
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
  `user/…/playlist/…` form) → `https://open.spotify.com/{kind}/{id}`; Apple Music →
  `https://music.apple.com/{country}/{kind}/{id}` without the name slug, an album URL with
  `?i={id}` naming that track (`/song/{id}`), the iTunes Store's `itunes.apple.com/…/id{N}` too; YouTube and YouTube Music (`youtu.be`,
  `m.`/`music.youtube.com`, `/shorts/`, `/embed/`) → `https://www.youtube.com/watch?v={id}` and
  `…/playlist?list={id}`; Deezer without the language path → `https://www.deezer.com/{kind}/{id}`;
  TIDAL (`/browse/`, `listen.tidal.com`, the `/u` share suffix; tracks, albums, artists, videos) →
  `https://tidal.com/{kind}/{id}`; Qobuz store album
  pages → `https://open.qobuz.com/{kind}/{id}`; SoundCloud (`m.`, any letter case, parameters) →
  `https://soundcloud.com/{user}/{track}`; Amazon Music (`albums/{id}?trackAsin=` naming that
  track; Amazon's own hosts only, `music.amazon.com`, `.co.uk`, `.de`, `.fr`, `.it`, `.es`, `.ca`,
  `.com.au`, `.co.jp`, `.in`, `.com.br`, `.com.mx`) → `https://music.amazon.{tld}/{kind}/{id}`; Bandcamp without parameters. Short links
  that need a request to expand (`spotify.link`, `deezer.page.link`, `on.soundcloud.com`) are not
  folded.
- **Wikipedia articles**: the mobile host, `index.php?title=…` (a revision with `oldid` too),
  spaces and percent-encoding are aliases of `https://{lang}.wikipedia.org/wiki/{Title}`. A
  redirect's title stays its own link.
- **The page's own canonical**: after a fetch, a page whose head canonical link or
  all-permanent redirect chain names a URL that differs from it only by a trailing slash and tracking
  parameters becomes an alias of that URL. A canonical to another path (a homepage, a sibling
  page), another host or another query (`/events/?event_id=42` naming `/events/`) never
  aliases.

Refresh follows the time to an event's start: more than 30 days or no start, 7 days; 7–30 days,
2 days; 24 hours to 7 days, 6 hours; under 24 hours or running, 1 hour
(`limits.refresh_resource_secs`, `limits.refresh_container_secs`). Adapter floors can lengthen
these; OSM follows `limits.osm_ttl_secs`. One fetch at end + 24 hours (start + 24 hours without
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
from then on; the changes of an alias's ID are its own and its root's. A fetch failure emits nothing and never cancels a resource.
A calendar snapshot with more than `limits.feed_max_changes` (100) occurrence changes emits one
feed-level `details` line with `fields: ["occurrence_changes"]`: re-read the occurrence window.
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
capped by the `event_*` limits.

**Changes and disappearance.** Rows are compared by SEQUENCE, LAST-MODIFIED and a normalized
content hash, so DTSTAMP and fetch noise create no changes. Explicit cancellations apply at
once; disappearance needs two fresh complete snapshots, and roll-off past the horizon or past
end + 24 hours never cancels. A new UID with identical content supersedes the old row without a
cancellation. A snapshot that loses more than half its active keys, or adds more than
`limits.feed_max_new_keys` (1000) to a feed with rows, is held until an identical snapshot
confirms it; a feed over `limits.feed_max_rows` (10000) is held until it shrinks, and
`limits.feed_max_holds` (5) consecutive holds dead-letter it. A re-parse that moves instances
(for example by an hour across a DST change) re-keys them once, under the same rules. A feed
answering HTML, 404 or 410 counts as an empty snapshot: two of them cancel its rows and mark it
missing, while its history stays readable here. Cancelled, superseded and past rows are purged
`limits.feed_retention_days` (30) after their end.

Calendar validators (ETag, Last-Modified) are sent only after a saved snapshot with nothing
held or pending and more than half the forward window left; otherwise an unconditional GET
fetches a body. A 304 renews the TTL but neither confirms a disappearance nor extends the
horizon. Mixed dated and undated RSS, Atom and JSON feeds keep their dated items.

**Pubky event references.** Nexus `kind:event` references carry `recurrence_id`, start and end
(Unix milliseconds) and an optional lifecycle. The reference key includes the author, object
path, venue and recurrence, so deleting one instance leaves the others. Invalid keys or
timestamps and an end before the start are skipped and counted in
`pinakes_ingest_rejected_total{reason="event"}`.

**NIP-52 calendars.** A calendar waits (`calendar_members_pending`) for members not yet
resolved, at most `limits.calendar_member_wait_hours` (48) after it first listed them. A member
still unresolved then, one whose resolution failed, or one without a start is skipped with
`calendar_member_unavailable`: the other members apply, and the skipped member's rows are
cancelled after two snapshots. A calendar's link to a member it no longer lists ends at once,
and a listed member's link lives 24 hours, renewed at every fetch of the calendar, so a calendar
that rotates its members never runs into its cap and the links of one nobody fetches lapse.

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
  (`mb:…`), Wikidata item (`wikidata`), Discogs artist, master and release (`discogs:…`), IMDb
  title (`imdb`), Open Library edition and work (`openlibrary:…`), podcast GUID and Apple podcast
  ID (`podcast:guid`, `podcast:apple`): a join that would bring a second (a wrong ID on a hub) is
  refused, so one bad claim never chains two groups. ISRCs and barcodes may be several.
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
evidence count again. Invalid curation returns 400. `groups.enabled = false` turns off group
reads, writes and collapse; stored membership stays for when it is turned back on.

## Possibly the same event

One meetup is often listed on several sites (Meetup, Luma, its organiser's page). These pages
share no identifier, so they never [group](#groups). Instead an event's card names other events
that are possibly the same one, in `maybe_same`: `{id, uri, site, start, score}`, best first, up
to three on a card and ten in its `details`. An entry is an event resource, or one occurrence of
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
whenever an event's start, place or name changes; a request only reads them. A new or changed
event is paired within about five seconds of its save, once the events before it in the queue
are read (the events already stored when the hint is installed are queued at once). Search
pages change their `ETag` when a hint on them changes. `[event] same_event_hint = "off"`
([operations](operations.md#calendars-and-events)) removes the field from cards, details and the
OpenAPI document and stops the pass.

## Also on

A card's `also_on` says where else the same thing is: first the other members of its
[group](#groups) (links someone tagged, each with its resource `id`), by ID and URI, then
**derived links**: the same thing on services nobody tagged, found by a source, with
`derived: true`, the service's name in `site`, the hub or service it came through in `via`
(`musicbrainz`, `wikidata`), and for some a `kind` (`archived`) and a time `at`. Derived links
are ordered by service name; each URL appears once (a tagged member wins) and never the card
itself. A card carries at most 8 entries, and one link per service (by `site`): for a music
service a tagged member first, because someone used that link (of several, the one on the
recording's original album, the `original_album` fact), then the derived link most group members
name; none on the card's own service (an artist's card never shows another page of its service,
such as the band's earlier name, which Wikidata may list as a second ID). Tagged members that are
not music always show. `GET /v1/resources/{id}` shows the same, up to 256 entries, and
lists every link in `details.also_on` (several of one service too). A derived link can be unavailable in some
countries (a regional release on a streaming service); nothing public says where, so prefer a
tagged member.

A derived link is a hint: it is never fetched, never counted as a reference and never ranks
anything. Sources read a resource after it is saved and again when their results age (each
source's lifetime, typically 30 days); a source's links and keys live twice that, so one failed
refresh keeps them, and they go when the source stops returning them. A source's exact keys
(an MBID, a Q-id) also [group](#groups) the links that share them. A derived link found through
an exact ID (a MusicBrainz relation, a Wikidata item's claim or sitelink, a service's answer to
a lookup by barcode or ISRC) carries that ID: a tagged resource that is that link (the same URL
after its aliases) takes it too and joins the group, even when its own lookup finds nothing.

**Music.** A track, album or artist link on a music service ([music service links](#aliases))
is looked up exactly, never by title or name. A key comes from a hub (MusicBrainz, Wikidata) or
from a known music service's own record of its own object; another host's claim (its JSON-LD
`isrcCode`, `sameAs`, a canonical link) is a fact at most.
- `service`: the ISRC a music service's own track page states (its JSON-LD `isrcCode`), as the
  key `isrc:…` (`via` the service) only from a service whose catalogue comes from labels and whose
  pages were checked to state it: TIDAL. Elsewhere it is the fact `isrc` (on Bandcamp and
  SoundCloud an uploader types it), and not when MusicBrainz gives that ISRC to several recordings
  (it names none of them). A track whose ISRC nobody else knows (not MusicBrainz,
  not another link) stays alone with its key, and joins as soon as another link or MusicBrainz
  carries the same ISRC.
- `musicbrainz`: what MusicBrainz relates the link to (one recording, release or release group,
  or artist; several are ambiguous and give nothing), that entity's current links on music
  services, and its MBIDs, the recording's ISRCs (up to eight) and the release's barcode as keys
  (`mb:recording:…`, `mb:release:…`, `mb:release-group:…`, `mb:artist:…`, `isrc:…`,
  `upc:…`). A track MusicBrainz does not relate by URL takes the recording its page's own ISRC
  is on when that ISRC is a key (TIDAL, `service` above) and MusicBrainz knows exactly one. An album link related to up to four releases of one
  release group (editions MusicBrainz keeps apart) takes the release group's key alone, and the
  links, facts and barcode every one of those releases has. Its facts: `mbid`, the recording's
  `isrc`s, the release's barcode as `upc`, the `artist` credit, the recording's `duration` and
  `original_album` (the earliest official album that is no compilation, live or remix album),
  and an artist's `artist_type` (`Person`, `Group`, …). ISRCs are stored in upper case and barcodes as
  GTIN-13 (a 12-digit UPC gains a leading zero), by every source.
- `deezer` (off unless the operator turns it on: Deezer's API terms allow non-commercial use
  only): a Deezer track's or album's own ISRC or barcode from Deezer's API as a key, and for
  another service's track or album with an ISRC or barcode key, the one Deezer track or album
  the API names for it, keyed by it.
- `itunes`: for an album with a barcode key, the Apple Music album iTunes' `lookup?upc=` names
  when exactly one album answers, keyed by that barcode (so a tagged link to that album joins).
- `wikidata`: the one item carrying the link's own service ID (Spotify, Apple Music, Deezer,
  TIDAL, Qobuz, YouTube, SoundCloud, Amazon Music, Bandcamp), read back and compared exactly
  (two items are ambiguous and give nothing); its other music-service IDs as links and its Q-id
  and MusicBrainz ID as keys. A track takes only an item that is one recording (one MusicBrainz
  recording ID, no work ID): a song item is a work that its studio, live and remastered
  recordings share, so a track takes neither its Q-id nor its tracks.

**Wikipedia.** An article's card comes from its Wikipedia's REST summary (`Article`, the title,
the first paragraph up to 600 characters, the thumbnail, `site` `Wikipedia ({lang})`), so a long
article whose page exceeds the page cap still gets one; a page the summary does not cover (a
special page) is read as a web page. `wikipedia`: the article's own Wikipedia names its Wikidata item (following
redirects); the item's other language editions are listed as `Wikipedia ({lang})`, the major
languages first, with its Britannica, Stanford Encyclopedia of Philosophy, Internet Encyclopedia
of Philosophy, Scholarpedia and Namuwiki entries, and its Q-id is the key, so two editions of one
article join. Titles are never matched across languages.

**Wikidata items.** `wikidata_item`: a Wikidata item link (`/wiki/Q…`, `/entity/Q…`,
`Special:EntityPage`, the mobile host: all aliases of `https://www.wikidata.org/wiki/{Q-id}`) is
its own hub. Its card lists the item's encyclopedia entries, the music services its best-rank
IDs name, and all its Wikipedia editions (the major languages first), with the Q-id as key. Only
the item's own claims and sitelinks count, never a search.

**Books.** `books`: an ISBN (`isbn:…`, `openlibrary.org/isbn/{isbn}`) or an Open Library edition
page (`openlibrary.org/books/OL…M`) is looked up exactly, never by title:
- Open Library's edition record must list the ISBN asked. Its edition and work IDs are keys
  (`openlibrary:edition:…`, `openlibrary:work:…`, so the editions of one work join, a hint like
  every group), its ISBN-13s are `isbn` facts, and it links the edition, its work
  (`kind: "work"`), its Internet Archive copy (`ocaid`), and its Goodreads, LibraryThing and
  Google Books pages when it holds exactly one ID for each.
- A printed edition's 978 ISBN gives its Amazon page (`amazon.com/dp/{ISBN-10}`, `via: "isbn"`).
- An ebook edition's ISBN gives its Apple Books page when iTunes lists exactly one ebook for it.
  A printed edition's ISBN is never asked: iTunes answers it with loose matches, such as an
  edition in another language.
- The work's Wikidata item (by its Open Library work ID) gives its Q-id as a key and its
  LibraryThing, Goodreads (the work's editions), Google Books, Project Gutenberg and BookBrainz
  pages.

Goodreads' `book/isbn/{isbn}` redirect is never used: it answers an unknown ISBN with an
unrelated book.

**Podcasts.** `podcasts`: an Apple Podcasts link (`podcasts.apple.com/{cc}/podcast/{slug}/id{n}`
in any spelling, an episode with `?i={n}`) or a podcast feed's card (a `PodcastSeries`, or a
`PodcastEpisode` with its item's GUID) is looked up exactly:
- iTunes must know an Apple ID as a show. The show's key is `podcast:apple:{id}`, and it links
  Apple Podcasts and the players that open a show by its Apple ID (Pocket Casts, Castro).
- A show's GUID is its `podcast:guid:{uuid}` key and its `podcast_guid` fact. A feed card takes
  the GUID its feed names (read at the card's feed URL, never through a redirect); a feed that
  names none gets the GUID derived from its URL, as the podcast namespace says.
- With an operator's Podcast Index key ([operations](operations.md#also-on)), Podcast Index joins
  an Apple ID to its feed and a feed to its Apple ID, and links the show there. Its answer must
  name the ID or feed URL asked, and an Apple ID it names counts only once iTunes knows it.
- An episode's key is its Apple track, `podcast:apple_episode:{id}`, with its Apple Podcasts
  page: a feed item is found among the show's latest 200 episodes on iTunes by its GUID.
- The show's Wikidata item (by its Apple ID) gives its Q-id as a key and its Spotify, Podcast
  Index, Podchaser, Pocket Casts and Castbox pages.

**Films and series.** `films`: an IMDb title (`imdb.com/title/tt…`), a TMDB movie or series, a
Letterboxd film or a TVmaze show is looked up exactly, never by title:
- The IMDb title ID is the join, as the key `imdb:{tt…}`. An IMDb link names it, TVmaze's record
  of a show names it, and a TMDB or Letterboxd link reaches it through the one Wikidata item
  with that link's ID.
- The title's Wikidata item (by its IMDb ID) gives its Q-id as a key and its IMDb, TMDB,
  Letterboxd, Trakt, Rotten Tomatoes, TheTVDB, Apple TV, Netflix, MUBI, TVmaze, MyAnimeList and
  AniList pages.
- Letterboxd and TVmaze are asked by IMDb ID only when the item has no link there: Letterboxd's
  `imdb/{tt}/` redirects to its film (an unknown ID answers a page, not a redirect), and
  TVmaze's `lookup/shows?imdb=` to its show, whose record must name the same IMDb ID. TVmaze's
  data is CC BY-SA; its links say `via: "tvmaze"`.

A title never lists its own service.

The level is exact: a track lists tracks and videos of the recording, an album lists albums and
YouTube's album playlists, an artist lists artist pages and channels. Album links group by
their release group, while two editions (two MusicBrainz releases) stay apart. Apple Music, Apple Books and Apple Podcasts links use the storefront
`[also_on] apple_storefront` (`us`).

## Cards and resolvers

Public `/v1/info` names the kinds this instance resolves in `resolves`; the admin `/v1/info`
lists one manifest per resolver and card kind (`media`, `isbn`, `doi`, `nostr`, `activitypub`,
`official`),
the event platform adapters, worker status and the effective limits.

- **ISBN**: ISBN-10/13 checksums, metadata from Open Library.
- **DOI**: `doi:`, `https://doi.org/…` and `https://dx.doi.org/…` request CSL JSON with the
  publication date and container. Invalid bare identifiers are final `missing`; informational
  DOI URLs fall back to web.
- **Nostr**: profiles and notes through the signed-event indexer; NIP-52 events keep their
  event schema; unknown signed kinds give a generic card. Relays (`event.adapters.websocket`)
  are asked only after both HTTP indexers miss.
- **ActivityPub**: advertised endpoints or ActivityStreams JSON give Person, Article or
  SocialMediaPosting cards only when the fetched URL and the object ID share a registrable
  domain. Note excerpts are capped at 200 characters; unlisted content is omitted.
- **Media**: oEmbed (`media.oembed`) and page JSON-LD, which wins when healthy. An oEmbed answer
  survives a failed or truncated page. Podcast types need a namespace or an audio/video
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

## Sources

Where a service's page is walled, thin or moved, Pinakes reads the service's own keyless API for
its links instead: the card's `details.from` names the source and `details.facts` holds the
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

Every public `/v1` route and Swagger UI share a per-IP budget on the direct peer address
(`api.requests_per_minute`, 120; forwarded headers are not trusted). A reach search costs
`limits.reach_cost` (6) units, charged before any Nexus call; every other request costs one.
Over budget returns 429 with `Retry-After: 60`. Public queries use their own ten-connection
pool with a five-second statement and acquisition timeout. The dump is written by the daemon,
not by a request: its writer's read-only transaction lifts the statement timeout to drain a large
snapshot, and the API only serves the file.

Handler errors are `{"error": "…"}`; Axum's query and JSON parse errors may be text. Malformed
query parameters and JSON return 400, JSON of the wrong shape 422, bodies over 2 MB 413.
Internal errors return 500 without database details, and a handler panic is answered 500 by a
backstop layer (logged with its path, counted in `pinakes_api_panics_total`), never a dropped
connection. There are no resource write, social-state, reverse-geocoding or place-search
routes.

## Admin listener

`admin_listen` (default `127.0.0.1:9090`) serves `GET /metrics` (Prometheus text),
`GET /v1/info` with full ingestion, source and canary state, `GET /v1/resources/{id}/document`
(the stored document with its internal fields) and group curation (`PUT /v1/groups/{id}`). With
`admin_token` (or `PINAKES__ADMIN_TOKEN`) every admin route needs `Authorization: Bearer`,
compared in constant time; a non-loopback bind without a token is refused. It sends no CORS
headers. Public `/v1/info` shows only what a client needs (the version, the kinds of links this
instance resolves, the lookup sizes, the search page limit, the per-IP and preview budgets and
the OSM licence); the admin `/v1/info` adds manifests, resolver status, counts, ingestion,
sources and metrics. Errors are stored only as sanitized classes. Admin info and database
metrics are cached for 15 seconds. Metrics and alerts are described in [ops](../ops/README.md).

Canaries (`canary.enabled`) probe each enabled adapter every `limits.canary_interval_secs`
(6 hours) under the ordinary worker and host budgets. A web canary denied by host policy is
skipped; OSM probes bypass the cache without writing it. A gauge appears only after the first
run.
