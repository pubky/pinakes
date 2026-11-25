# Pinakes API v1

Pinakes serves typed metadata (cards) for the external resources people reference on Pubky:
search and ranking over those links, calendar dates, groups of links to the same thing, and the
changes to the links a client keeps. It does not serve social state. Clients read posts, tags and
users from Nexus in a separate call and join the two by URI: send a card's `uri` and `refs`
(never its `id`, which is Pinakes's own). In short, Pinakes ranks and Nexus hydrates.

| | |
| --- | --- |
| Base URL | The Pinakes origin. Every path here is relative to it (`$BASE/v1/search`). |
| Document | OpenAPI 3.1 at `GET /v1/openapi.json` ([snapshot](openapi.json)); Swagger UI at `/swagger-ui` |
| Authentication | None on the public listener (`listen`). The admin listener (`admin_listen`) takes a bearer token ([Admin listener](#admin-listener)). |
| Rate budget | 120 units per minute per client address by default (`api.requests_per_minute`), shared by every public route ([Limits and errors](#limits-and-errors)) |
| CORS | Any origin by default (`api.cors_allow_origins = ["*"]`), no credentials ([CORS](#cors)) |
| Errors | JSON `{"error": "…"}` with the HTTP status, on the public `/v1` routes ([Errors](#errors)) |
| Card shape | One flat object per card (`api_shape: 2` in `/v1/info`) |

## Basics

### Document and Swagger UI

The public listener serves its OpenAPI document at `GET /v1/openapi.json`. Swagger UI at
`/swagger-ui` (`api.swagger_ui`, on by default) renders it from a vendored bundle, so **Try it
out** reaches only public operations. Every operation carries named examples
([Examples](#examples)).

The admin listener serves no document; its routes are listed under
[Admin listener](#admin-listener). `scripts/openapi` regenerates the snapshot in this
repository, and a test fails while the snapshot differs from the code.

### Authentication

The public listener takes no credentials: no keys, cookies or tokens. It writes nothing except
budgeted previews (`POST /v1/resolve`) and the queue position of a known resource that a lookup
moves to the front (`POST /v1/resources/lookup`).

The admin listener needs `Authorization: Bearer <admin-token>` on every route except
`GET /healthz` when `admin_token` is set ([Admin listener](#admin-listener)).

### CORS

`api.cors_allow_origins` decides which browser origins may call the public listener:

| Value | Behavior |
| --- | --- |
| `["*"]` (default) | Every response, including 429s and requests without `Origin`, carries `Access-Control-Allow-Origin: *` |
| A list of `scheme://host[:port]` origins | Only those origins get the headers (the origin is echoed). Every response carries `Vary: Origin`. |
| `[]` | No CORS headers |

Allowed responses expose `Retry-After` and `ETag` (`Access-Control-Expose-Headers`). A
preflight (`OPTIONS` with `Access-Control-Request-Method`) gets 204 with
`Access-Control-Allow-Methods: GET, POST`, `Access-Control-Allow-Headers: content-type` and
`Access-Control-Max-Age: 600`, and spends the per-IP budget. A refused origin gets 204 without
CORS headers. Pinakes never sends `Access-Control-Allow-Credentials`, and the admin listener
never sends CORS headers.

### Caching

Search pages and resource reads carry `Cache-Control: public, max-age=30` and an `ETag`.
Send the `ETag` back in `If-None-Match` to get 304 when nothing changed. The OSM dump has its
own caching rules ([OpenStreetMap and locations](#openstreetmap-and-locations)).

### IDs and URIs

Pinakes normalizes every URI and computes its resource ID itself:

- A URI that parses as a URL is serialized as a URL: the scheme and host are lowercased, the
  default port, user info and fragment are dropped, an empty path becomes `/`, dot segments
  (`.`, `..`) are resolved and the path is percent-encoded. The query is kept in its order.
- A URI without an authority (`nostr:`, `bitcoin:`, `isbn:`, …) keeps its text without the
  fragment, and a `bitcoin:` payment request also without its query.
- A URI that does not parse as a URL only has its scheme lowercased; the rest is kept exactly.
- The ID is the first 16 bytes of the BLAKE3 hash of the normalized URI, as 32 lowercase hex
  characters.
- A URI is at most 2,048 bytes.

IDs are Pinakes's own, so combine Pinakes with other indexes by URI. A missing upstream object
is a known resource with `status: "missing"`, not a 404.

`POST /v1/resources/lookup` never fetches an unknown URI. A known resource whose refresh is due
moves to the front of the refresh queue; its TTL, backoff and final state still apply.

## Routes

### Public routes

| Method and path | Input | Response |
| --- | --- | --- |
| `GET /v1/search` | Query parameters ([Search](#search)) | `{items: [card], cursor, reach_truncated, facets?}` |
| `GET /v1/resources/{id}` | A 32-character resource ID | The card with the [read's fields](#resource-read); 404 if unknown |
| `GET /v1/resources/by-uri` | `uri`: a link | As `GET /v1/resources/{id}` for the URI's ID ([By URI](#by-uri)); 404 if unknown. Nothing is fetched or stored. |
| `POST /v1/resources/lookup` | Exactly one of `{"ids": […]}` (alias `resource_ids`) or `{"uris": […]}`, 1–500 entries | `[card]` in request order, duplicates kept, unknown entries skipped ([Lookup](#lookup)) |
| `POST /v1/resolve` | `{"uri": "https://example.org/"}` | The card with the read's fields, within 3 seconds; else 202 with `status: "pending"`. 404 when the resource expired while the request waited; 429 or 503 with `Retry-After` over a [preview budget](#preview-budgets) ([Resolution and status](#resolution-and-status)) |
| `GET /v1/resources/{id}/occurrences` | Optional `from`, `to`, `limit`, `cursor` | `{items: [occurrence card], cursor}` ([Calendars and occurrences](#calendars-and-occurrences)) |
| `POST /v1/occurrences/lookup` | `{resource_ids?, uris?, from?, to?, limit?, cursor?}`: 1–500 feed IDs and links together | `{items: [occurrence card], cursor}` across the feeds, by start |
| `GET /v1/resources/{id}/episodes` | Optional `limit`, `offset`, `q` | `{items: [episode], next_offset, truncated}`: a podcast show's latest episodes ([Episodes](#episodes)); 404 if unknown |
| `GET /v1/resources/{id}/collections` | Optional `viewer`, `limit`, `cursor` | `{items: [{uri, author, indexed_at}], cursor}`: the public collections that hold the card's link ([Collections](#collections)); 404 if unknown |
| `GET /v1/groups/{id}` | A 32-character group ID | `{id, type, curated, keys, members: [card]}` ([Groups](#groups)); 404 if unknown; 400 for a malformed ID or while groups are off (`groups.enabled = false`) |
| `POST /v1/changes/lookup` | `{resource_ids?, uris?, since?, cursor?, limit?}`: 1–500 IDs and links together | A page of [change lines](#change-line) ([Changes](#changes)) |
| `GET /v1/info` | None | `{version, api_shape, user_agent, resolves, limits, osm_licence, attributions?}` ([Info](#info)) |
| `GET /v1/dumps/osm_place.ndjson.gz` | None | The ODbL derivative database as a daily gzip file ([OpenStreetMap and locations](#openstreetmap-and-locations)) |
| `GET /v1/openapi.json` | None | The OpenAPI 3.1 document |
| `GET /swagger-ui` | None | Swagger UI, when `api.swagger_ui` is on |
| `GET /` | None | The index for agents: links to the guide, the skill, the MCP endpoint, this API's document, `/v1/info` and nexus-scout ([Agents](#agents)) |
| `GET /llms.txt` | None | The usage guide for agents, as plain text ([Agents](#agents)) |
| `GET /skill.md` | None | The same guide as an agent skill named `pinakes` ([Agents](#agents)) |
| `POST /mcp` | One MCP JSON-RPC message | The MCP endpoint, when `api.mcp` is on ([Agents](#agents)). Not in the OpenAPI document. |
| `GET /mcp` | None | When `api.mcp` is on: for a browser (`Accept: text/html`), a short page on how to connect; any other GET gets 405 with `Allow: POST` |

### Admin routes

| Method and path | Response |
| --- | --- |
| `GET /healthz` | 200 `ok`, or 503 ([Admin listener](#admin-listener)); takes no token, but the [Host check](#admin-listener) applies |
| `GET /metrics` | Prometheus text |
| `GET /v1/info` | The operator's state: ingestion, sources (canary state included), resolvers and queue statuses, counts and metrics ([Admin info](#admin-info)). Not the public info's fields. |
| `GET /v1/resources/{id}/document` | The stored document with its internal fields |
| `PUT /v1/groups/{id}` | Replaces a group's members ([Curation](#curation)) |
| `DELETE /v1/groups/{id}` | Undoes a curation |

### By URI

`GET /v1/resources/by-uri?uri=…` answers with the card for the URI's ID. An alias answers with
its root's card ([Aliases](#aliases)).

A spelling held neither as a card nor as an alias answers with its root by the static alias
rules, when that root is stored. These rules need only the URI: tracking parameters dropped, a
`doi.org` or ISBN link, a service's other URL forms. Otherwise the answer is 404, and nothing is
fetched or stored. To have an unknown link read, use `POST /v1/resolve`.

### Lookup

`POST /v1/resources/lookup` returns up to 500 cards in one request. Send exactly one list:

- `ids` (or `resource_ids`): resource IDs. An alias's ID answers with its root's card.
- `uris`: links, each read as `by-uri` reads it, static alias rules included.

The answer lists the cards in request order. Duplicates are kept and unknown entries are
skipped. Sending both lists, or neither, is a 400.

### Info

`GET /v1/info` tells a client what it needs to know about the instance:

| Field | Meaning |
| --- | --- |
| `version` | The daemon's version |
| `api_shape` | `2`: one flat object per card |
| `user_agent` | The User-Agent this instance's requests to other services carry: its version and the operator's contact (`http.contact`), when set |
| `resolves` | The kinds of links this instance resolves: `web`, `pubky`, `geo`, `osm`, `event`, `calendar` and the card kinds `media`, `isbn`, `doi`, `nostr`, `activitypub` and `official`, those that are on |
| `limits` | The sizes and budgets a client must respect: `lookup`, `change_lookup` and `occurrence_lookup` (500 each), `search_page` (500), the `related_*` limits ([Related](#related)), `collapse_refs_per_member`, `requests_per_minute`, `previews_per_minute` and `previews_per_uri_per_minute` |
| `osm_licence` | `{licence, attribution, url, dump, updated_at}`: the ODbL licence of OSM-derived values, the dump's path, and when the dump was last written (`null` before the first) |
| `attributions` | What a client must show while it shows values from a service, `[{provider, notice, logo, url}]` ([Credits](#credits)). Absent when there are none. |

The admin listener's `/v1/info` is a different object, the operator's state
([Admin info](#admin-info)), without these fields.

## Response shapes

Every route returns the same card for a resource or an occurrence. Absent values are `null`,
never `""`.

Text fields (`name`, `description`, `site`, and every text a read adds) are plain text. Pinakes
decodes the source's HTML entities once, so escape the text before you render it as HTML. A
source text that holds markup (a page's JSON-LD `description`, a DOI's title with `<i>`) is read
as text, with its tags dropped.

A post's `description` (X, Mastodon, Bluesky, Threads, Telegram, Reddit, Instagram) keeps the
post's line breaks (`\n`, at most one blank line in a row), with the spaces inside a line
collapsed. Its `name` is the start of the text on one line.

### Card

| Field | Meaning |
| --- | --- |
| `id` | The resource ID; for an occurrence, the feed's ID |
| `uri` | The resource URI. For an occurrence, its own URL when it has one, else the feed's. To combine with Nexus, send `uri` and `refs`, and for an occurrence `occurrence.feed_uri` (nobody tagged the occurrence's own URL). |
| `refs` | The links people referenced on Pubky that resolve to this card, at most 5 ([refs](#refs)). `[]` when nothing references the card. |
| `collections` | How many public collection posts hold the card's link: its own `uri` or one of its aliases' (the 31 most recently collected). Each post counts once, up to 1,000 (`1000` means 1,000 or more); `0` when none does. `GET /v1/resources/{id}/collections` lists them ([Collections](#collections)). An occurrence's are its feed's. |
| `status` | Fetch health: `ok`, `pending`, `missing`, `unreachable` or `error` ([Resolution and status](#resolution-and-status)) |
| `type` | The schema.org type (`MusicEvent`, `Book`, `Place`, …), or one of Pinakes's own types for what schema.org has no class for: `BitcoinBlock`, `BitcoinTransaction`, `BitcoinAddress`, `LightningNode` and `LightningChannel` (each a child of `Thing`) |
| `type_path` | The type's ancestors, root first (`["Thing", "Event", "MusicEvent"]`): the types a `type=` search matches it by |
| `name`, `description` | Display text |
| `image` | An image URL. Images point to their origin and are never proxied. A web image (`https` or `http`) is on a named host: never an IP address, `localhost`, a single-label name or a link with a user. |
| `stream` | Audio a client can play, `{url, type, duration_s, preview}` ([Stream](#stream)). Absent when the card has none. |
| `embed` | The frame that shows the link as its own site does, `{provider, kind, src, height, aspect, allow, autoplay_param}` ([Embed](#embed)). `null` when there is none and on a `missing` card. |
| `episodes` | On a podcast show's card (`PodcastSeries`, status `ok`) only: its latest episodes that have a `stream`, newest first, at most 5 ([Episodes](#episodes)). Absent on every other card and on a show without one. |
| `site` | The publishing site ([site](#site)) |
| `point` | `{lat, lon}` |
| `start`, `end` | RFC 3339 instants, for events and occurrences |
| `group` | `{id, size}` when the resource or occurrence is in a [group](#groups), else `null`. `size` counts this member too. |
| `also_on` | The same thing elsewhere ([Also on](#also-on)): the other members of its group, then derived links nobody tagged. At most 8 entries on a search, lookup or group card and 256 on a resource read. A hint only: every member keeps its own card. |
| `open_in` | The clients that open the card, `[{client, url}]` ([open_in](#open_in)) |
| `attribution` | Present on OSM cards, on events placed at an OSM venue and on events whose time zone came from coordinates. Display it with the card. |
| `credits` | Where `image` and `description` came from when Pinakes added them from another source than the link: one entry per such field, at most 4 ([Credits](#credits)). `[]` when none: such a field came from the link itself. |
| `occurrence` | Occurrence cards only: `{feed_id, feed_uri, uid, recurrence_id, status}` ([Occurrence](#occurrence)) |
| `maybe_same` | Other cards that are possibly the same thing, best first, each `{id, uri, site, start, score}`: for events ([Possibly the same event](#possibly-the-same-event)), app store cards ([Possibly the same app](#possibly-the-same-app)) and `MusicRecording` cards ([Possibly the same recording](#possibly-the-same-recording)). Up to 3 on a search, lookup or group card and up to 10 on a resource read; `[]` when there are none. Absent on other cards, and on a kind's cards when its hint is off. |
| `because` | [Related](#related) search rows only: why the row is listed, `{shared, taggers}`. Absent on every other card. |
| `labels` | Search rows only: the labels the card's catalogue gives it, as valid tag labels, kept apart from people's tags (which Nexus serves for `uri` and `refs`); a client may offer them as suggested tags. `[]` on every other search row ([Catalogue cards](#catalogue-cards-in-search)). Absent on other routes. |

#### refs

`refs` lists the links people referenced on Pubky that resolve to the card: its own `uri` and
the aliases that carry references (of the 31 most recently referenced aliases). They are ordered
by how often they were referenced, counted up to 1,000 each, then by URI. At most 5 are listed,
normalized as every URI is.

A typed card's `uri` is often a link nobody wrote (`btc:block/…`, `uniprot:…`, `isbn:…`), while
people tagged the mempool.space, UniProt or Open Library page. Ask Nexus about each of `refs`
and add up the card's tags over them.

An occurrence's `refs` are its feed's. A gone social post's `refs` name its own link alone.

#### site

`site` names the publishing site:

- a web page: the page's own site name (`og:site_name`), else its host;
- a `pubky://` object: the app's path segment (`loopky`), not the public key;
- a link whose URL names a known service (a music service, a Wikipedia edition, MusicBrainz,
  Wikidata, a service the also-on lookups know): that service's name (`Apple Music`,
  `Wikipedia (de)`, `OpenStreetMap`), whatever the page calls itself and even before the card is
  read;
- a card read from an official API without a host of its own: the service's name
  (`Federal Register`).

#### open_in

`open_in` lists the clients that open a card, as `[{client, url}]` with `https:` URLs:

- A Pubky app's object: the clients from the app's profile ([Pubky apps](pubky-apps.md)), such
  as Loopky for a deck.
- A card whose link is not a web page (`btc:`, `ln:`, `uniprot:`, `fr:`, …): the web page its
  source reads it from, named by its site, such as
  `[{"client": "mempool.space", "url": "https://mempool.space/block/…"}]`.
- `[]` on a web link's card, on a card gone upstream and on a card whose source names no page.

#### How links are typed

A music link is typed by what it is, whatever the service's page or player calls it, on every
service ([music service links](#aliases)):

| Link | Type |
| --- | --- |
| A track | `MusicRecording` |
| An album | `MusicAlbum` |
| An artist | `MusicGroup`; `Person` from the next refresh after MusicBrainz says the artist is one |
| A playlist | `MusicPlaylist` |

YouTube is not only music:

- A channel is a `MusicGroup` only when it is a Topic channel (named `{Artist} - Topic`) or on
  YouTube Music.
- A playlist is a `MusicPlaylist` only on YouTube Music (`music.youtube.com`, the tagged spelling
  or another spelling of the same playlist, from the card's next read).
- Any other channel or playlist keeps the type its page gives.
- A video stays a `VideoObject`, except a label's upload: a video whose channel is an artist's
  Topic channel, or whose description starts "Provided to YouTube by", is a `MusicRecording`.
  Its fact `artist` is the Topic channel's name, else the first artist of the description's
  "Title · Artist" line. This holds whichever spelling of the link was read; `music.youtube.com`
  alone does not make a video a song, because an official music video plays there too. Its
  `embed` stays YouTube's video player.

Any other YouTube or Vimeo video has its channel as the fact `author`. A video's `duration` fact
(seconds) is there when Pinakes read its length: from the page, or from the watch page read to
find a label's upload on other music services. YouTube can answer the watch page with a
sign-in page instead, so the length may be missing.

A track's, album's or artist's page keeps its own video (a `VideoObject` in its JSON-LD) as a
part of the page, not its card ([Lists](#lists)). The card's `site` is the service's name
(`Spotify`, `Apple Music`, `TIDAL`, `YouTube`, …) in any page language, as derived links show it.

A Bandcamp root or `/music` page features an album but is the artist's page: a `MusicGroup`
named after the artist, without the album's description.

When the service's own page names the artist (JSON-LD `byArtist`, as TIDAL's pages do), the
description says what the page says instead of its share text. A track's reads
`Queen · A Night At The Opera (Deluxe Edition) · 5:55` (artist, album, length), an album's
`Tame Impala & JENNIE · 2025` (artist, year). The `service` source adds `artist`, `album` and
`duration` (seconds) to `facts`.

A media link is typed the same way:

| Link | Type |
| --- | --- |
| A TMDB, Letterboxd or Trakt movie | `Movie` |
| A TMDB, Trakt or TVmaze series | `TVSeries` |
| An Open Library work or edition, a Goodreads work or book | `Book` |
| An Apple Podcasts, Podcast Index, Pocket Casts, Castro or Fountain show | `PodcastSeries`; its episode a `PodcastEpisode` |
| A Netflix title | `Movie` or `TVSeries` when its Wikidata item says which, else `CreativeWork` |
| An IMDb title | The type its page gives (it can be either) |

A Wikidata item that is a creative work is that work (`TVSeries`, `Movie`, `Book`, …), never an
`Event`, whatever dates it carries: a series' run is not an occurrence. An item is a creative
work when it is an instance of (P31) a series, a film, a book, an album, a song, a game, a
podcast or a periodical; else when it has a publication date, an author, a director, a composer,
a publisher or an episode count. A page that is a podcast episode by its feed
([Also on](#also-on)) is a `PodcastEpisode` from its next refresh.

Such a card is named after its object, not its page. Where a TMDB, Trakt, Netflix, Goodreads,
Letterboxd, Open Library, TVmaze or Podcast Index page names its site, another language's title
or nothing, the name comes from the object's exact record. So does the description, where the
page's text is not the object's. The record is the Wikidata item that carries the page's own ID,
or the service's own record (`media_records`, [docs/sources](sources/README.md)).

That record is read while the page is, within the page hook's limit (at most 3 seconds, 6 for a
walled page). When Wikidata is slower, the card keeps the page's name at first. The `titles`
source then reads the same item without that time limit. When the card bears another name, it
has the link read again, and the card takes the item's label (and description, where the page's
is not the object's).

#### Pubky objects and gone posts

A `pubky://` object of an app Pinakes has a profile for (Loopky, Pubky Rooms, the Pubky
marketplace) gets that app's card; any other app's object gets a general one
([Pubky apps](pubky-apps.md)). An object without a name, title or description takes its
description from a content-like member (`content`, `text`). Such a card is a
`SocialMediaPosting`, social content like a note: a 200-character excerpt that refreshes within
the hour and is removed once the object is gone ([social content](design.md#social-content)).

A social post found gone keeps nothing of itself. This applies to a note, and to a post,
comment, question or profile an official-API source reads. Its card has `status: "missing"`,
its `uri`, `site` and `from`, and the type `SocialMediaPosting`, whatever the post was (a Reddit
thread, a TikTok video). It has no name, description, image or facts, no also-on links and no
change history. Its read lists no `aliases`, and its `refs` name its own link alone, because a
spelling's link can carry the post's title.

### Stream

`stream` is one field for everything playable, so a client needs one player for episodes and
recordings:

```json
"stream": {"url": "https://…/ep180.mp3", "type": "audio/mpeg", "duration_s": 3557, "preview": false}
```

| Field | Meaning |
| --- | --- |
| `url` | The audio itself, for an `<audio>` element; never a player page. Pinakes never fetches or proxies it. |
| `type` | Its MIME type without parameters: always `audio/…` or `video/…`, at most 127 bytes |
| `duration_s` | Its length in whole seconds when its source states it, else `null`. A preview's is usually `null`; the recording's own length is the fact `duration`. |
| `preview` | `true` for a short sample of a longer recording (a music service's 30-second preview); `false` for the whole of it (an episode) |

Only a public, direct link is a stream:

- `https` or `http` on a named host: no IP address, `localhost` or single-label name, and no
  user or password;
- no query parameter that signs the link or makes it expire (`sig`, `hdnea`, `hmac`, `auth`,
  `expires`, `exp`, `policy`, `Key-Pair-Id`, any `X-Amz-…` or `X-Goog-…`, any name with `token`
  or `signature` in it), so a link that stops working, or works only for whom it was made, is
  never one;
- at most 2,048 bytes;
- a stated length of at most a week.

A feed or page behind a login is never read, so nothing behind one is a stream. A publisher's
tracking redirect (`podtrac.com`, `pdst.fm`) is its public link and stays. No DRM service's audio
is a stream: Spotify's, Apple Music's and TIDAL's own players are not streams.

Which cards carry one (each source's page in [docs/sources](sources/README.md) says how):

- A podcast feed's episode (`{feed}?guid={item}`, or the show's own episode page whose feed item
  links back to it): the item's first audio or video `<enclosure>`, with its `itunes:duration`.
- A Podcast Index episode: the enclosure Podcast Index records (the `podcast_index` source).
- A Castro episode: the file its page plays (the `castro` source).
- Any other episode the also-on `podcasts` source reads (an Apple Podcasts or Fountain episode,
  a page its feed's item links back to): the enclosure it finds, from the card's next read (the
  source asks for that read at once).
- An Apple Music track: iTunes' 30-second preview, `preview: true` (the `itunes` source), from
  the card's next read. Deezer's previews are signed and expire, so they are never a stream;
  Spotify's are not used.

### Embed

`embed` is the frame a client may load, sandboxed, to show a link the way its own site does. A
client needs no list of services to build one.

```json
"embed": {"provider": "spotify", "kind": "audio", "src": "https://open.spotify.com/embed/track/003vvx7Niy0yvhvHt4a68B",
          "height": 152, "aspect": null,
          "allow": ["accelerometer", "autoplay", "clipboard-write", "encrypted-media", "gyroscope", "picture-in-picture", "web-share"],
          "autoplay_param": null}
```

| Field | Meaning |
| --- | --- |
| `provider` | Who serves the frame: `youtube`, `vimeo`, `spotify`, `soundcloud`, `mixcloud`, `tidal`, `deezer`, `apple-music`, `apple-podcasts`, `x`, `instagram`, `mastodon` or `openstreetmap`; else the oEmbed provider's name in lower case with hyphens (`speaker-deck`); else the frame's host |
| `kind` | `audio` or `video` (a player), `post` (a social post), `map`, or `rich` (anything else: slides, a document, code, a chart) |
| `src` | The frame's address. The frame never starts playing by itself. |
| `height` | In CSS pixels (at most 2,000), for a frame of fixed height (a player strip, a post, a document); `null` when the frame scales with its width or the provider gives no height |
| `aspect` | Width divided by height, for a frame that scales with its width (a video, a map, a slide deck; 16:9 when the provider names no size); `null` otherwise |
| `allow` | The Permissions Policy features for the frame's `allow` attribute (join them with `; `), only from `accelerometer`, `autoplay`, `clipboard-write`, `encrypted-media`, `fullscreen`, `gyroscope`, `picture-in-picture` and `web-share` |
| `autoplay_param` | A query parameter (`autoplay=1`, `auto_play=true`) that makes the player start once loaded. Set it in `src` only after the viewer pressed play: add it, or replace that parameter's value there, which is off. `null` when the player has none; a player may still take its own start message (Spotify's, TIDAL's). |

Pinakes takes the frame from the first of these that has one:

1. **The link itself**, when the card is served, so every stored card has it:
   - a YouTube video, playlist or channel (its uploads), a Vimeo video;
   - a Spotify track, album, artist, playlist, show or episode;
   - a SoundCloud track, set or profile, a Mixcloud show;
   - a TIDAL track, album, video or playlist, a Deezer track, album, playlist or artist;
   - an Apple Music song, album or playlist, an Apple Podcasts show or episode;
   - an X post;
   - an Instagram post or reel: Instagram's captioned frame of its root,
     `https://www.instagram.com/p/{code}/embed/captioned/` or `…/reel/{code}/embed/captioned/`, from
     any spelling of the link, whether Pinakes could read the post or not;
   - a Mastodon post on the server it was posted on (`/@{account}/{id}/embed`), when Mastodon's
     own API answered for it;
   - an OpenStreetMap card's map (its bounding box, or the streets around its point, with a
     marker).

   `also_on` links get these frames as well.
2. **The provider's oEmbed answer**, read with the card. Pinakes takes only the `src` of its
   first `<iframe>` (or a Mastodon-style `data-embed-url`), never its HTML, with the iframe's
   `width`, `height` and `allow`. It asks a provider of the pinned registry
   (`data/oembed-providers.json`) when the page names no player of its own; only the frame is
   taken, and the card keeps the page's fields. Another site's oEmbed answer counts when the
   card reads it anyway.
   - The `provider` is the answer's `provider_name` as a slug (`speaker-deck`), else the frame's
     host. An answer takes the name of a service from step 1 only for a frame on that service's
     own embed host (never Mastodon's).
   - An answer from an endpoint only the page named (on the page's own origin, not the
     registry's endpoint for the link) is the site's own word: its `provider` is the frame's
     host, and the card's `provider` fact is the endpoint's host.
3. **The page's own player**: its head's `twitter:player` (with `:width` and `:height`), or an
   `og:video:secure_url` or `og:video:url` whose `og:video:type` is `text/html`.

A frame from step 2 or 3 never starts by itself. An autoplay its address turns on (`autoplay=1`)
is turned off there and given as `autoplay_param`. A frame without such a parameter has no
`autoplay` in `allow`.

Other rules:

- A card whose link names no frame takes the first frame its other links name (its `http:`
  spelling, a short link).
- A tagged `also_on` member whose link names no frame shows the frame its own card shows.
- A `missing` card, and a post found gone, have no frame.
- A `rich` or `video` frame with a fixed height stored on a card of an audio type (a
  recording, an album, a podcast or episode) is an `audio` player. A `rich` frame on a social
  post is a `post`.

Only a safe address is an embed:

- `https` on a public host name: no IP address, `localhost`, single label, private suffix such as
  `.local` or `.internal`, or name on a well-known service that resolves names to private
  addresses (`nip.io`, `localtest.me`);
- no user, password or port, and no credential in the query or fragment;
- at most 2,048 bytes.

`javascript:`, `data:` and any other scheme are never an embed. Frames from steps 2 and 3 come
with a card's next read; `pinakesd reextract media` reads the oEmbed providers' cards again at
once.

### Credits

A card can show an image or a description that Pinakes added from another source than the link.
Each such field has a credit in `credits`. Show the credit with the value, linked.

- A MusicBrainz card can show an image from Wikimedia Commons and a description from Wikipedia
  or Wikidata ([MusicBrainz enrichment](#musicbrainz-enrichment) says which item and which text).
- A Wikidata item's card shows its item's Commons image with the same `image` credit
  ([Cards and resolvers](#cards-and-resolvers)). Its description is the item's own and has no
  credit.
- A release's card can show its front cover from the Cover Art Archive.
- A film's or series' card can show TMDB's poster and overview.

```json
"credits": [
  {"field": "image", "provider": "wikimedia_commons",
   "source_url": "https://commons.wikimedia.org/wiki/File:Miles_Davis_by_Palumbo_cropped.jpg",
   "author": "Tom Palumbo from New York City, USA", "license": "CC-BY-SA-2.0",
   "license_url": "https://creativecommons.org/licenses/by-sa/2.0/", "lang": null,
   "excerpted": false, "modified": false, "subject": null},
  {"field": "description", "provider": "wikipedia",
   "source_url": "https://en.wikipedia.org/wiki/Miles_Davis", "author": "Wikipedia contributors",
   "license": "CC-BY-SA-4.0", "license_url": "https://creativecommons.org/licenses/by-sa/4.0/",
   "lang": "en", "excerpted": true, "modified": false, "subject": null}
]
```

| Field | Meaning |
| --- | --- |
| `field` | `image` or `description`: the card field the value is in |
| `provider` | `wikipedia` (an article's summary), `wikimedia_commons` (a file), `wikidata` (an item's English description), `cover_art_archive` (a release's front cover) or `tmdb` (a film's or series' poster or overview) |
| `source_url` | An `https:` URL of at most 1,024 bytes: the article; the file's page on Commons; the item's page on Wikidata (`https://www.wikidata.org/wiki/Q…`); for a cover, the cover art page of the release it is of on MusicBrainz (`https://musicbrainz.org/release/…/cover-art`); or the title's page on TMDB (`https://www.themoviedb.org/movie/…` or `/tv/…`) |
| `author` | Plain text, at most 256 bytes: `Wikipedia contributors` for an article, the file's author for an image. `null` for a CC0 or public-domain file that names none, for a Wikidata description, for a cover and for TMDB's values. |
| `license` | An SPDX licence ID (`CC-BY-SA-4.0`, `CC-BY-2.0`, `CC-BY-SA-3.0-DE`, `CC0-1.0`, `GPL-3.0-only`, `LGPL-2.1-or-later`), `public-domain`, or `Attribution` (Commons' licence of that name, which has no SPDX ID). `null` for a cover and for TMDB's values. |
| `license_url` | The licence's text (`https:`): for a GNU licence, its version's text on gnu.org; for `Attribution`, its template's page on Commons. `null` for the public domain without a mark, for a cover and for TMDB's values. |
| `lang` | The text's language: `en`, or another Wikipedia's code (`de`) when only that Wikipedia describes it; `null` for an image. A client may label or hide a text in a language its reader does not read. |
| `excerpted` | `true` when the text is a part of a longer source: a summary is the article's first paragraphs, and one over 2,048 bytes is cut at a sentence or a word. A Wikidata description is whole (`false`). |
| `modified` | `true` when the value was changed beyond cutting it |
| `subject` | The link of what the value is about, when that is not the card itself: a recording's description taken from its work's item names the work (`https://musicbrainz.org/work/…`). Else `null`. |

A credit is there only with its value: a card without the image or description has no credit for
it. A link that carries a credential ([Links that carry a credential](#links-that-carry-a-credential))
is never a credit's source, and its value is not taken, so nothing the store drops can leave a
value without its credit. The same credits are on every card of the resource: a search, lookup
or group card, a resource read and the MCP tools' cards.

#### Commons licences

A Commons file under a GNU licence is credited with the licence every Commons template of that
name grants:

| Commons template | `license` |
| --- | --- |
| `GPLv3` (written for version 3 only, and for version 3 or later) | `GPL-3.0-only` |
| `GPL` | `GPL-2.0-or-later` |
| `AGPL` | `AGPL-3.0-only` |
| `LGPL` with gnu.org's `licenses/lgpl.html` | `LGPL-2.1-or-later` |
| `LGPL` with gnu.org's `copyleft/lgpl.html` | `LGPL-3.0-or-later` |
| `LGPL` without a URL | `LGPL-2.1-only` |

A name that says "or later" (`GPLv3+`) is credited so.

#### Wikidata descriptions

```json
{"field": "description", "provider": "wikidata",
 "source_url": "https://www.wikidata.org/wiki/Q55825905", "author": null, "license": "CC0-1.0",
 "license_url": "https://creativecommons.org/publicdomain/zero/1.0/", "lang": "en",
 "excerpted": false, "modified": false, "subject": null}
```

#### Cover Art Archive

A release's or release group's card shows the front cover the
[Cover Art Archive](https://coverartarchive.org/) holds for it when the card has no free Commons
file, or when its Commons file could not be read just now and the card has no stored image. The
credit names the release the cover is of; a release group's cover is one of its releases', chosen
by the archive.

A recording can take a front cover from a release that contains its exact MusicBrainz recording
ID: one release page (at most 100 releases), with at most three Cover Art Archive requests, under
the shared cache, host pacing and enrichment deadline. A front flag alone is not an image; the
archive must confirm it. The release's barcode and album identity stay separate from the
recording.

```json
{"field": "image", "provider": "cover_art_archive",
 "source_url": "https://musicbrainz.org/release/e7ba3cb7-a074-45ee-870f-3baeb6d3e8bf/cover-art",
 "author": null, "license": null, "license_url": null, "lang": null,
 "excerpted": false, "modified": false, "subject": null}
```

#### TMDB

A film's or series' card (`Movie`, `TVSeries`) with no image or no description of its own can
show TMDB's poster and overview. They are found by the title's IMDb ID or Wikidata item alone
(the `tmdb` source, [docs/sources](sources/README.md#also-on)), and only on an instance with a
TMDB key. The credit names the title's page on TMDB and no author or licence; the overview's
`lang` is `en`.

```json
{"field": "image", "provider": "tmdb",
 "source_url": "https://www.themoviedb.org/movie/157336",
 "author": null, "license": null, "license_url": null, "lang": null,
 "excerpted": false, "modified": false, "subject": null}
```

A client that shows a value credited to `tmdb` also shows TMDB's logo and notice. `/v1/info`
gives both in `attributions` while the instance uses TMDB, and while a stored card still holds a
TMDB value:

```json
"attributions": [
  {"provider": "tmdb",
   "notice": "This product uses the TMDB API but is not endorsed or certified by TMDB.",
   "logo": "https://www.themoviedb.org/assets/2/v4/logos/v2/blue_short-8e7b30f73a4020692ccca9c88bafe5dcb6f8a62a4c6bc55cd9ba82bb2cd95f6c.svg",
   "url": "https://www.themoviedb.org"}
]
```

A card drops TMDB's values when they were last read 60 days ago and TMDB has not answered since,
or when the instance stops using TMDB.

### Resource read

`GET /v1/resources/{id}`, `GET /v1/resources/by-uri` and `POST /v1/resolve` return the card with
the fields below beside the card's own, in one flat object (`api_shape: 2`; there is no
`details` object). A read's `also_on` lists up to 256 links, one per service, and its
`maybe_same` up to 10. The two `GET` reads carry `Cache-Control: public, max-age=30` and an
`ETag` of the body, and answer `If-None-Match` with 304.

| Field | Meaning |
| --- | --- |
| `category` | The OSM `key/value` category, or the resolver's category |
| `address` | `{street, house_number, postcode, city, country_code}`, the parts present |
| `bbox` | `[min_lon, min_lat, max_lon, max_lat]` |
| `tz` | An event's IANA time zone |
| `all_day` | Whether the event is all-day (its `end` is then exclusive) |
| `event_status` | `scheduled`, `cancelled`, `postponed`, `rescheduled` or `moved_online` |
| `attendance` | `offline`, `online` or `mixed` |
| `venue` | `{name, id}`: the venue's name and, when it resolved to a resource, its ID |
| `organizer` | `{name, url}` |
| `from` | Where the card's metadata came from when it is not the page itself: the [source](#sources) whose official API answered, such as `federal_register` |
| `facts` | Named values about the object ([Facts](#facts)). `null` when there are none. Facts never join anything. |
| `related` | The objects the card points at, each with its own card ([Related objects](#related-objects)); `[]` on cards that point at none |
| `related_total` | How many entries of a `rel` there are when `related` lists fewer (`{"input": 400}`); `null` when it lists them all |
| `keys` | The public identifiers the resource is keyed by, as `GET /v1/search?key=` takes them (`isrc:…`, `upc:…`, `doi:…`, `isbn:…`, `mb:recording:…`, `wikidata:Q…`, `osm:node/…`): the keys its sources vouch for ([Also on](#also-on)), sorted. `[]` when there are none. |
| `aliases` | Other spellings of this link that resolve to it ([Aliases](#aliases)), at most 50. A page's own `url`, `@id`, `sameAs` or canonical claims are not aliases and are not listed. The card's `uri` is the canonical one. |
| `updated_at` | When the current metadata was accepted |

Nothing that describes the fetch rather than the thing is public: the resolver, TTL, group keys,
raw attributes, parser warnings, content type and length. Operators read the stored document
with `GET /v1/resources/{id}/document` on the admin listener.

#### Facts

`facts` holds named values, several values of one name joined with `; `:

- the fields an official API names for the object (`document_type`, `citation`, …);
- the facts a [Pubky app's](pubky-apps.md) profile reads (`card_count`, `price`, `rating`, …);
- a MusicBrainz page's own facts (`artist`, `duration` in seconds, `genre`, …:
  [MusicBrainz facts](#musicbrainz-facts));
- a YouTube or Vimeo video's (`artist` or `author`, `duration`: [How links are typed](#how-links-are-typed));
- then the facts the [also-on](#also-on) sources read for it, such as a music link's `isrc`,
  `upc` and `mbid`.

Each source keeps at most `limits.also_on_facts_per_source` (32, at least 8) facts per resource.
It cuts the many values of one name (a recording's ISRCs) before a name's one value (its MBID,
artist). Each source's page in [docs/sources](sources/README.md) lists its facts.

### Related objects

A resource read's `related` lists the objects its card points at, so a client can show them and
open each one's card. Each entry is `{rel, uri, name?, amount_sat?, at?}`:

| Field | Meaning |
| --- | --- |
| `rel` | What the object is to the card (table below) |
| `uri` | Its card's root, which `POST /v1/resolve` reads (and `GET /v1/resources/by-uri` answers once it is known): a chain card's root, a podcast show's feed, or a show's Podcast Index page, never another page on a site. `null` for an input or output without an address. |
| `name` | Plain text: a block's "Block 975,251", a node's alias, a show's title, or what an entry without a `uri` is (`OP_RETURN`, `P2PK`, `coinbase`, or the script type) |
| `amount_sat` | An amount in bitcoin base units (BIP 177, ₿; ₿100,000,000 is 1 BTC) |
| `at` | An RFC 3339 instant |

A `rel` lists at most 10 entries, and `related` at most 32 in all. When there are more,
`related_total` gives their number (`{"input": 400}`). Entries come in the order of the table;
inputs, outputs and channels come largest first.

The list is read with the card and stored with it (from mempool.space, or from an episode's feed
or its page's source). It changes when the card is read again, never on a read of the resource.
It is not the `related=` search ([Related](#related)), which ranks what a link's taggers also
referenced.

| Card | `rel` |
| --- | --- |
| `BitcoinTransaction` | `block` (with `at`, the block time); `input` and `output` (an address, or `null` with a label; `amount_sat`) |
| `BitcoinBlock` | `previous`; `next` (once the next block is six deep in the best chain); `coinbase` (its first transaction; `amount_sat` is the block reward) |
| `BitcoinAddress` | `tx`: its latest transactions, those in the mempool first. `amount_sat` is what each moved into (positive) or out of (negative) the address, and `at` is there once confirmed. |
| `LightningNode` | `channel`: its biggest open channels (`amount_sat` the capacity, `name` the peer's alias); `peer`: those channels' other nodes, each once |
| `LightningChannel` | `node` (both, with their aliases); `funding_tx` (`amount_sat` the capacity); `closing_tx` once closed |
| `PodcastEpisode` | `show`: its show's card ([below](#the-show-of-an-episode)) |

Roots keep their network: a testnet4 transaction's inputs are `btc-testnet4:address/…`, a signet
channel's funding transaction `btc-signet:tx/…`.

```json
"related": [
  {"rel": "block", "uri": "btc:block/0000000000000000000008dfb288144ec4626cad43656ff2cdd8cb278886fe80",
   "name": "Block 975,251", "at": "2026-11-10T03:32:51Z"},
  {"rel": "input", "uri": "btc:address/1FRQ1b96RVdsGbJUHHiqNGVSnuxq1cMBAZ", "amount_sat": 994797},
  {"rel": "output", "uri": "btc:address/17Hh59K259oHaMtSfeJh9UmkV4seDAQe7x", "amount_sat": 129015985}
],
"related_total": {"input": 400}
```

#### The show of an episode

A `PodcastEpisode` card's `show` entry depends on where the episode was read:

- From the show's feed (the episode's feed item, or a page that names its feed): the feed's
  address without `guid`, never one that carries a credential, with the feed's title as `name`.
- From a Podcast Index episode: the show's Podcast Index page, with the show's title Podcast
  Index records as `name`.
- From a Fountain episode page: the feed its page names.

### Occurrence

An occurrence card is a card with `occurrence: {feed_id, feed_uri, uid, recurrence_id, status}`:

| Field | Meaning |
| --- | --- |
| `feed_id` | The feed's resource ID (the card's `id` too) |
| `feed_uri` | The feed's URI: the link people tagged. Send this to Nexus, not the card's own `uri`. |
| `uid` | The instance's UID in the feed |
| `recurrence_id` | `null` for a non-recurring instance; else a UTC instant or an all-day date |
| `status` | The instance's lifecycle: `scheduled`, `cancelled`, `postponed`, `rescheduled` or `moved_online`. The card's own `status` is the feed's fetch health. |

Use `(feed_id, uid, recurrence_id)` as the client's row key: `id` alone names the feed.

### Group

`GET /v1/groups/{id}` returns `{id, type, curated, keys, members}`:

| Field | Meaning |
| --- | --- |
| `id` | The group ID |
| `type` | The type most members have (ties by name) |
| `curated` | `true` when an operator set the members ([Curation](#curation)) |
| `keys` | The identifiers two or more members share (`cite:…`, `ics_feed:…`, `ics_uid:…`); none for a group an operator set |
| `members` | Every member's own card, by ID and then member (an occurrence is its own member) |

### Change line

A change line is `{seq, resource_id, occurrence?, at, class, fields, root?}`:

| Field | Meaning |
| --- | --- |
| `seq` | The line's position in the journal; pages ascend by it |
| `resource_id` | The resource that changed |
| `occurrence` | For an occurrence change: the serialized `[uid, recurrence_id]` key |
| `at` | When, in Unix milliseconds |
| `class` | What changed ([Changes](#changes)) |
| `fields` | Every changed field |
| `root` | On an `aliased` line only: the resource that answers for this one |

A line names what changed, not the new values: read the current card for those.

## Examples

This catalogue has one request per value. It covers every kind of card and source Pinakes
handles: each event platform's pages, calendars and feeds, OSM in each accepted form, web pages,
media, books and articles in their spellings, social posts, Pubky objects of other apps, and the
`missing`, `unreachable` and `pending` states. A source's own examples are on its page in
[docs/sources](sources/README.md).

Each request below is a named example in Swagger UI (`/swagger-ui`), named by its value, except
the episodes and collections requests. To run one, pick it in the dropdown of a body or a
parameter and use **Try it out**. Every parameter of `GET /v1/search` starts at `(none)`: pick
values in the parameters you want (a map viewport sets `bbox`); the others are not sent.

Paths are relative to the instance's base URL, for example `curl "$BASE/v1/search?q=lugano"`.
POST bodies are JSON:

```sh
curl -H 'Content-Type: application/json' -d '{"uri":"https://example.org/"}' "$BASE/v1/resolve"
```

Resource IDs are the deterministic hashes of the URIs, so the requests return data on any
instance that indexed them.

Times may be relative to the request, so the time windows and change lookups never go stale.
Search's `from` and `to`, and the change lookup's `since` (a string, or an integer of Unix
milliseconds), accept `now`, `now+30d` or `now-12h`: days or hours from the current minute.
Later pages of one search resolve them at the instant its cursor pins, so the window does not
move between pages. In a raw query string `+` decodes to a space, so `now 30d` is accepted as
`now+30d`.

Three caveats:

- Search lists only links someone referenced on Pubky, so the search examples need the URIs
  tagged. Previews, cards, groups, dates and changes do not.
- The reach viewer is a placeholder; use a user that exists in the Nexus the instance reads.
- A group ID is the one a group of those members gets when it first forms. On an instance where
  the group formed differently it has another ID; take `group.id` from either member's card.

The examples live in `data/api-recipes.json`. `crates/pinakes-core/tests/recipes.rs` runs every
one against a fixture of the same URIs and checks that this section lists every recipe's request
line.
`scripts/e2e --live-recipes` (also run by `--isolated-live`) resolves each `/v1/resolve` example
live and checks its card's type, so an example whose page has gone stale fails there.

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
| `POST /v1/resolve {"uri":"https://infomaniak.events/en-ch/festivals/fete-de-la-biere-2027/75fb7215-0440-42a2-bee9-1af1bf0aa857/events/394429"}` | Event |
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
GET /v1/search?facet=host
GET /v1/search?facet=category&type=Place
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
GET /v1/search?bbox=8.90,45.98,9.00,46.03
GET /v1/search?bbox=8.45,47.33,8.62,47.42
GET /v1/search?bbox=13.25,52.45,13.55,52.58
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
GET /v1/resources/faf081897e81377fd45b404f02a7f190/episodes?offset=20&limit=20
GET /v1/resources/faf081897e81377fd45b404f02a7f190/episodes?q=khabib
```

### GET /v1/resources/{id}/collections

```text
GET /v1/resources/faf081897e81377fd45b404f02a7f190/collections?limit=5
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

`GET /v1/search` lists the links people referenced on Pubky and the cards of the catalogues that
are on, filtered and ranked. Every parameter is optional, and the filters combine. A resource is
listed only with at least one public reference ([Catalogue cards](#catalogue-cards-in-search) for
the cards nobody referenced).

The answer is `{items, cursor, reach_truncated, facets?}`:

| Field | Meaning |
| --- | --- |
| `items` | The page's cards |
| `cursor` | The signed token of the next page; `null` on the last page ([Paging](#paging)) |
| `reach_truncated` | `true` when the `reach` or `boost` set was capped at 1,000 authors |
| `facets` | With `facet`: counts over the whole filtered set ([Facets](#facets)) |

### Parameters

| Parameter | Default | Meaning |
| --- | --- | --- |
| `q` | None | Text in the name, other names or description, at most 256 bytes ([Text search](#text-search)) |
| `type` | Any type | A schema.org type with its subtypes (`Event` matches `MusicEvent`; `Place` matches cafés and other `LocalBusiness` types), or up to 10 comma-separated: any of them (`type=MusicRecording,MusicAlbum,MusicGroup`) |
| `exclude_type` | None | Leaves out a schema.org type with its subtypes (`exclude_type=Event` leaves out `MusicEvent` too), or up to 50 comma-separated, spelled as for `type`. A link with no type yet (not read, or unreachable) is left out as well. Combines with `type`: `type=CreativeWork&exclude_type=Article`. |
| `category` | None | One category exactly, at most 256 bytes: a place's OSM `key/value` (`amenity/cafe`), or the `category` a page's schema.org data gives |
| `host` | None | A host exactly as the links' URLs name it (`www.youtube.com` and `youtube.com` are two hosts), or up to 10 comma-separated: links on any of them (`host=open.spotify.com,www.deezer.com`). Spaces around a host are ignored; each host is at most 256 bytes. |
| `key` | None | Links carrying one exact identifier, on any service and in any spelling ([Search by key](#search-by-key)) |
| `related` | None | A resource ID: what the people who tagged it also referenced ([Related](#related)). Not with `sort` or `boost`. |
| `tags` | None | Comma-separated labels: at most 5, each 1–128 bytes. They match people's tags and catalogue cards' labels. |
| `match` | `any` | With `tags`: `any`, or `all` (at most 3 tags) |
| `author` | None | Resources referenced by this public key (52 characters, z-base-32) |
| `reach` | None | `following`, `friends` or `wot_2` of `viewer`: only their references and tags count (a filter; [Reach and boost](#reach-and-boost)) |
| `boost` | None | `following`, `friends` or `wot_2` of `viewer`: cards they referenced rank higher, and nothing is left out ([Reach and boost](#reach-and-boost)). Only with the default ranking, `sort=relevance` or `sort=trending`. |
| `viewer` | None | The Nexus user (a public key) whose set `reach` and `boost` read; required with either |
| `venue` | None | Events at this venue resource ID |
| `in_collection` | None | The cards a public collection holds: the collection post's `pubky://` URI, as an item's `uri` from `GET /v1/resources/{id}/collections` ([Collections](#collections)) |
| `remote` | None | `true`: [job postings](#jobs) that may be done remotely; `false`: job postings that may not |
| `closed` | `false` | `true`: list [job postings](#jobs) past their closing date too; they are left out otherwise |
| `ingredient` | None | [Recipes](#recipes) with all these ingredients: up to 3, comma-separated, each 1–64 bytes of letters, digits, spaces and hyphens. Each matches whole words of an ingredient's name as the card records it (`flour` matches "all-purpose flour", `mozz` matches nothing). |
| `sort` | `relevance` with `q`, else `activity` | `relevance` (needs `q`), `activity`, `recent`, `distance` (needs `near`), `start` or `trending` ([Ranking](#ranking)) |
| `window` | `7d` for trending; `limits.related_window_days` for related | With `sort=trending` or `related` only: `{n}h` or `{n}d`, from `1h` to `90d`, counted back from the first page's instant |
| `limit` | 50 | Cards per page, 1–500 |
| `cursor` | None | The previous page's `cursor` ([Paging](#paging)) |
| `facet` | None | Any of `type`, `tags`, `host` and `category`, comma-separated in any order ([Facets](#facets)) |
| `bbox` | None | `minLon,minLat,maxLon,maxLat` in degrees; west greater than east crosses the antimeridian. Not with `near`. |
| `near` | None | `lat,lon`; needs `radius`. Not with `bbox`. |
| `radius` | None | With `near`: positive metres, at most `limits.max_radius_m` (200 km by default) |
| `from`, `to` | None | Inclusive bounds on the start time: RFC 3339, Unix milliseconds, or relative (`now`, `now+30d`, `now-12h`; at most 100 years), within UTC years 0001–9999 |
| `collapse` | None | `group`: one row per group ([Collapsing groups](#collapsing-groups)) |
| `referenced` | None | `true`: only things someone referenced on Pubky, no catalogue card nobody referenced |

The answer is 400 for an unknown parameter, invalid coordinates or dates, an out-of-range
radius, an unknown type and incompatible options. A date outside years 0001–9999 gives the
message `timestamp out of range`. An unknown type's error names it when the name is 1–64 ASCII
letters and digits; anything else, such as a URL, gets a general message. Other statuses:

| Status | When |
| --- | --- |
| 304 | `If-None-Match` names the current `ETag` ([Search caching](#search-caching)) |
| 400 | `collapse=group` while groups are off (`groups.enabled = false`) |
| 501 | `reach=wot_2` or `boost=wot_2` on an instance whose Nexus serves no reach set |
| 502 | Nexus failed, or did not answer in time, while reading a `reach` or `boost` set |

### Text search

A search with `q` and none of `author`, `reach`, `related`, `in_collection`, `key`, `remote` and
`ingredient` reads its candidates from the text index ([operations](operations.md#text-index)):
the best `limits.search_text_candidates` (500) units for the page's order that match `q` by its
words, and the best 500 that Postgres' own text search may find otherwise, each with the filters
the index holds (`type`, `exclude_type`, `tags` and `match`, `host`, `category`, `venue`, `from`,
`to`, `near`, `bbox`, closed job postings, the catalogues that are on). The page, its facets and
`collapse` are built from those candidates, with every filter applied again and the exact
distance. `q` matches like this:

- Words: every word in the name, the description (an OpenStreetMap place's address line) or
  other names, as `"quoted phrases"` in order; `-word` leaves out the cards that hold it, and `or`
  between two words takes either. Letters match without their accents and case (`zurich` finds
  `Zürich`), and the last word also matches the start of a word in the name (`beatl` finds
  `Beatles`). A word is letters, digits and the marks they are written with; signs split words
  (`c++` is `c`, `node.js` is `node js`). In a script written without spaces (Chinese, Japanese,
  Thai) each character is a word, so `東京` matches `東京タワー`.
- As Postgres' text search, for a text that is not short: the text inside the name (`ILIKE`:
  `over` finds `Sovereign`) and a name similar to the text (`pg_trgm`'s `%`, similarity 0.3 or
  more: `kraftwerg` finds `Kraftwerk`, `daft punk` finds `Daft`). The index proposes the names
  that hold the text's three-letter pieces (at least 30% of them) and Postgres keeps those it
  matches, so a typo finds only near names. It proposes them only while fewer cards than its
  candidates (`limits.search_text_candidates`, 500) hold every word of the text in their name
  (with that many, the candidates are those cards), and leaves
  out a piece more than 10,000 names and 5% of the index hold, asking one piece fewer for each
  (one at least): a typo made of common pieces alone finds nothing this way.
- A short `q` (letters and digits only, at most three characters, counting one space between
  words: `ai`, `lol`, `btc`) matches a name only at the start of a word (`Airbnb`, not `Rain`; a
  sign such as `/` or `™` starts a word too) and the description by whole words. Several short
  words match in order, each at a word start (`a s` finds `A slow song`, not `As lop`).

People's tags and catalogue labels add to a card's relevance but never match `q` alone. A post
whose author has not opted in to search never matches: a read's `facts.opted_out` (Mastodon's
`indexable: false` or `noindex`, Bluesky's `!no-unauthenticated`). A card found by its words that
has no text any more (a post found gone or emptied since the index's last pass) is left out.

The index follows writes within `limits.search_index_commit_secs` (2 s): a new tag is found by
text a few seconds after Pinakes reads it, while a deleted or emptied card leaves every search at
once. With `q`, facets count the candidates, not every match, and a text search lists at most its
candidates over all its pages. Relevance uses the whole index's word statistics, so a commit
between two pages can move a card across the cursor: a later page may then repeat or miss a card,
as when its activity changes.

With `author`, `reach`, `related`, `in_collection`, `key`, `remote` or `ingredient`, and while the
daemon builds its text index after a start without one, `q` is matched in Postgres alone: its
words (`websearch_to_tsquery`, in the `simple` configuration, without accent folding), the text
inside the name and similar names, a short `q` as above.

### Catalogue cards in search

A catalogue card nobody referenced is a search row like any other, of a catalogue that is on
([catalogues](operations.md#catalogues)):

- found by text through the index, by `type` (with its subtypes), `exclude_type`, `tags` (its
  labels), `host`, `near` and `bbox`;
- `id` is the resource ID its `uri` would have, the same once someone references the link; `uri`
  is the card's canonical link; `labels` are its source's labels; `refs` is `[]`, `collections`
  0, `status` `ok`; `group`, `also_on` and `maybe_same` are empty. OpenStreetMap cards carry the
  ODbL `attribution`. `GET /v1/resources/{id}` answers 404 for it until someone references the
  link;
- a card whose `uri` a listed resource has is listed once, as that resource; a card whose
  resource the search leaves out (not referenced any more, or not yet read) is listed as the card;
- never listed with `referenced=true`, a social filter (`author`, `reach`, `related`,
  `in_collection`), `key`, `venue`, `category`, `remote`, `ingredient`, `from` or `to`, nor with
  `sort=recent`, `start` or `trending`, which rank by references or starts it has none of;
- without text, the best known first (by its source's fame) after every referenced unit, at most
  `limits.search_text_candidates` per search (by distance with `sort=distance`);
- a catalogue switched off leaves every search at once.

### Search by key

`key` finds the links carrying one exact identifier, on any service and in any spelling. Anything
else is a 400.

| Key | Matches |
| --- | --- |
| `mbid:{uuid}` | A MusicBrainz recording, release, release group or artist; also the MBID a MusicBrainz page was merged from (its `mbid_was` fact). With `type=MusicEvent`, an artist's events, by their performers. |
| `wikidata:{Q-id}` | A Wikidata item |
| `isrc:{code}` | A recording, in any letter case, hyphens allowed |
| `upc:{barcode}` | A UPC, EAN or GTIN in any of its lengths, check digit verified |
| `isbn:{ISBN-10 or ISBN-13}` | An edition |
| `imdb:{tt…}` | A film or series |
| `podcast:{GUID}` | A show |
| `podcast:enclosure:{URL}` | An episode, by the file it plays |
| `osm:{node\|way\|relation}/{id}` | An OSM object |
| `doi:{DOI}` | A DOI |
| `gtin:{barcode}` | A [product](#products), in any of its lengths |
| Any key as a resource read lists it in `keys` | `mb:recording:{mbid}`, `podcast:guid:{GUID}`, `podcast:apple:{id}`, … |
| Another namespace a source records | Each source's page in [docs/sources](sources/README.md#also-on) names its namespaces and how each is normalized |

A key matches the keys and facts the [also-on](#also-on) sources found, and the links an exact
lookup by that key named.

### Ranking

| `sort` | Order |
| --- | --- |
| `relevance` | How well `q` matches (below). The default when `q` is set, except with `related`, which ranks its own rows. Needs `q`. |
| `activity` | The activity score (below). The default without `q`. |
| `recent` | The newest reference first |
| `distance` | Nearest first; needs `near` |
| `start` | Earliest start first; units without a start are left out |
| `trending` | The references made inside `window`; only links referenced in it count. With `collapse=group`, one song across services is one row, ranked by the references all its links got in the window. |

Scores are internal and never returned.

**Activity** combines distinct taggers and references with a 30-day decay:

```text
(log(1 + taggers) + 0.5*log(1 + replies) + 0.3*log(1 + embeds + collections))
  * 2^(-age_of_newest_reference_in_days / 30)
```

**Relevance** ranks by how well `q` matches. From the text index:

```text
text * (1 + fame) + activity
```

- `text`: the match's BM25 score over the fields (the name weighing 3, people's tags 2, other
  names and source labels 1.5, the description 1), plus 20 when the name equals `q` (any case or
  accents, white space collapsed), BM25 again when the name holds `q`'s words in order, and 10 for
  the start of a name word; a card found as Postgres' text search finds it scores 8 for the text
  inside its name and 0.5 per shared three-letter piece;
- `fame`: a catalogue card's 0–1 fame from its source's own signal; 0 on a referenced link;
- `activity`: the activity score (with `boost`'s term when given). It grows with the log of the
  taggers, so each further tag adds less than the one before, and a thousand taggers (about 7) stay
  below an exact name match: tagged cards rank higher, they don't always come first.

With `q` and another `sort`, the index keeps the 500 matches that rank first in that order (the
most active, the newest reference, the earliest start, the nearest), and the page ranks them as
without `q`. With `sort=trending` it keeps the 10,000 matches (20 times
`limits.search_text_candidates`, at most 10,000) referenced last among those referenced inside `window`, and
Postgres ranks them by their references inside the window: a match whose last reference in the
window is older than those 10,000 is not listed. By activity, a catalogue card comes after every
referenced unit, the best known first.

In Postgres alone (with a social filter, `key`, `remote` or `ingredient`), relevance ranks the
name first, then the tags, then the description, and only then by activity:

```text
9 * name + 3 * tag + description + a / (1 + a)
```

- `name`: 4 when the name equals `q` (any case, white space collapsed); 3 when it holds `q`'s
  words as a phrase; 2 when it holds all of them; 1 for a partial or fuzzy match (`musical` for
  `music`, the trigram fallback, or for a short `q` a name word that starts with it: `Airbnb`
  for `ai`); else 0.
- `tag`: 2 when a label on the card equals `q` lowercased, also with its words joined by a space,
  hyphen, underscore or nothing (`daft-punk` for `daft punk`); 1 when a label holds `q`'s words
  as a phrase (`electronic-music` for `music`); else 0. The labels are the card's and its
  aliases', within `author` and `reach`, as for `tags`.
- `description`: 2 when it holds `q`'s words as a phrase; 1 when the name and description
  together hold all of them; else 0.
- `a`: the activity score (with `boost`'s term when given), mapped into [0, 1) so it only breaks
  ties.

Words are whole words there (`music` does not match `musical`), in the `simple` text
configuration, with no stemming. `q` alone decides what matches (name and description); tags
only rank.

### Paging

Pages are keyset-ordered on rank, then one internal key per resource or occurrence. A `null`
cursor ends the results.

The cursor is signed with a key created at daemon startup (HMAC-BLAKE3). It binds the filters,
the sort and the decay instant, so keep them unchanged; `limit` and `facet` may change between
pages. A tampered cursor, a cursor for other filters, or one from a previous daemon run gets 400:
start again without a cursor.

Concurrent updates show between pages; a search is not a frozen snapshot.

### Facets

`facet` adds `facets` to the answer: counts over the whole filtered set (with `q`, over the text
search's candidates; of catalogue cards, over the ones the search lists). The counts are the same
on every page, whatever `limit`, `cursor` or `sort`; `collapse` changes the rows, not the counts.
`tags` counts people's tags and catalogue labels together.
Each list is most frequent first, ties by value.

| Facet | Entries |
| --- | --- |
| `type` | `facets.type`: `{type, type_path, count}` per card type. `type_path` runs from `Thing` down to the type, enough to build the type tree and roll counts up into families. |
| `tags` | `facets.tags`: `{label, count}` per label people tagged the links with, at most 100 (the most frequent, ties by label) |
| `host` | `facets.host`: `{host, site, count}` per service, at most 100 ([below](#host-facet)) |
| `category` | `facets.category`: `{category, count}` per category, at most 100, as `category=` takes it: a place's OSM `key/value` (`amenity/cafe`), or the `category` a page's schema.org data gives |

Each value is what its filter takes, so a client can offer it as a choice. Filtering by a `tags`,
`host` or `category` entry finds exactly its count: one per card, an event occurrence counting as
its own card. A `type` entry counts that exact type. Because `type=` also finds subtypes, a
type's total is the counts rolled up along `type_path`.

A facet counts the set its own filter narrowed too. With `host=` set, `facets.host` lists only
the picked hosts, each with its own count, just as `type=` narrows `facets.type`. To offer every
choice next to the picked ones, read the facet from the same search without its own filter.

The `type` facet does not count links with no type yet. So a client that groups types can list
"every other type" as `exclude_type=` its groups' types.

#### Host facet

`host` is the host as `host=` takes it: exactly as the links' URLs name it, so `www.youtube.com`
and `youtube.com` are two entries. `site` is the name the cards show (`YouTube`,
`Wikipedia (en)`): the service Pinakes knows by the URL, else the site name the documents give
most often, else the host. A host that `pubky://` objects use is a user's key, not a service, and
is left out; `host=<user key>` still filters by it.

```text
GET /v1/search?facet=host&type=MusicRecording
→ facets.host: [{"host":"open.spotify.com","site":"Spotify","count":27}, …]
GET /v1/search?facet=category&near=46.0037,8.9511&radius=5000
→ facets.category: [{"category":"amenity/restaurant","count":16}, {"category":"amenity/cafe","count":9}, …]
```

### Collapsing groups

Without `collapse`, every member of a [group](#groups) is a row of its own, ranked on its own
references. `collapse=group` filters first, then keeps one row per group: the matching member
with the most references, counted up to `limits.collapse_refs_per_member` (10,000) each, ties by
the lower ID. That row is the member's own card, ranked as itself. Links in no group stay rows of
their own. Facets count every matching link either way.

### Reach and boost

`reach` filters: only references by authors in the viewer's set count, for the rows and for every
count.

`boost` ranks and never filters:

- With the default ranking, a card gains `2 · ln(1 + n)` for the `n` authors in the set who
  referenced it, halved every 30 days since the newest of their references. The card's own score
  decays the same way, so one recent reference from the set weighs about as much as three
  taggers.
- With `sort=relevance`, that term is added to the activity score that breaks ties between equal
  matches.
- With `sort=trending`, a reference in the window by an author in the set counts twice.

`boost` takes no other `sort` and no `related`. A search may give both `reach` and `boost`, each
with its own set.

The sets come from Nexus:

- `following` and `friends`: in pages of 200, at most 1,000 authors, cached per viewer for five
  minutes.
- `wot_2` (two steps of the follow graph): from Nexus's reach-set route,
  `GET /v1/user/{viewer}/reach-set?depth=2` (`{authors, truncated}`, at most 1,000, with an
  `ETag`). Only where the operator says the Nexus serves it (`nexus_reach_set = true`); else
  `wot_2` gets 501. The set is kept five minutes, then asked for again with its `ETag`, and kept
  while Nexus answers 304.

`reach_truncated: true` warns that a set was capped. All the pages a search reads, of its `reach`
and `boost` sets together, share `limits.reach_timeout_secs` (5 s). An incomplete list is not
cached. An unknown Nexus user has an empty set; a Nexus error or the deadline gives 502. Sets live
in the daemon's memory only, and nothing per viewer is stored.

### Search caching

Responses carry `Cache-Control: public, max-age=30` and an `ETag`. The `ETag` covers the
parameters, the resolved time window (and, for `sort=trending` and `related`, the minute of the
request), the ingestion cursors and epochs, a write revision (which the hint passes move too),
and the reach and boost sets' hashes and truncation. `If-None-Match` gets 304 when nothing
changed.

### Events and jobs in search

`type=Event` with `from`/`to` and `sort=start` covers event pages with a start and calendar
occurrences. Each is one result unit, so a calendar with three matching dates counts three
times. Pubky events are Nexus's ([Pubky events](#pubky-events)): a client merges them with these
results.

`type=JobPosting` with `near` or `bbox` finds postings by their workplace, and `remote=true` the
remote ones. `from` and `to` select by the date posted (a posting's `start`). A posting past its
closing date is left out of every search unless it sets `closed=true` ([Jobs](#jobs)).

## Related

`related={resource id}` lists what the people who tagged a resource also referenced: other
resources, ranked by how many of its taggers referenced them, then by the newest of those
references. It is what a "Fans also tagged" list or a "Because you tagged X" row needs. Like every
filter, it combines with `type`, `tags`, `q`, `reach`/`viewer`, `collapse=group` and `facet`. It
takes no `sort` (it is the ranking) and no `boost`.

A row needs at least two of the resource's taggers (`limits.related_min_shared`, 2), so the list
never mirrors one person.

Each row says why it is listed in `because`, `{"shared": n, "taggers": [...]}`:

- `shared`: how many of the resource's taggers referenced it (its rank's first term). With
  `collapse=group`, its group's members' taggers together, each tagger once.
- `taggers`: up to three of them as Pubky public keys, those who tagged the resource most
  recently first. Ask Nexus for their names and tags.

Both come from the sample the ranking counted ([Bounds](#related-bounds)), so `shared` counts
sampled taggers within the window. No other search carries `because`.

```json
{"id": "…", "uri": "https://example.org/kid-a", "name": "Kid A", "…": "…",
 "because": {"shared": 3, "taggers": ["<pubky key>", "<pubky key>", "<pubky key>"]}}
```

Rules:

- **Groups.** A resource in a [group](#groups) stands for the whole group: its taggers are the
  group's members' taggers, and the group's own members are never listed. With
  `collapse=group`, each row is one group, its members' taggers counted together. Without it,
  each resource counts its own.
- **References.** Only the references Pinakes copies from Nexus's feed count: the resource's
  taggers, and any reference of theirs (a tag, a reply, an embed, a collection). With `reach`,
  only the taggers within the viewer's reach.
- **Rows.** Each resource is listed once, as its own card, never once per occurrence of a feed.
- **Opted-out posts.** A post whose author has not opted in to search is never listed (as it is
  never found by its text), and references to it count for nothing: not toward its group, and its
  taggers are not the taggers of a group it is in.
- **Aliases.** An alias reads as its root; an unknown resource relates to nothing.
- **Cost.** A related search costs `limits.related_cost` (6) units of the
  [per-IP budget](#limits-and-errors).

### Related bounds

Every limit here is in `/v1/info` `limits`.

1. The taggers are drawn from a sample of the resource's most recent `limits.related_tag_sample`
   (2,000) tags within `limits.related_window_days` (365, or `window`).
2. There is one sample per member of its group and per alias of each, up to
   `limits.related_seed_links` (32) of those links, the ones with the newest tags.
3. All of it is taken before `reach` applies, and never filled back up when `reach` leaves fewer.
4. Of the sample's authors in reach, the `limits.related_taggers` (200) with the newest tag are
   sampled.
5. Of each, only their most recent `limits.related_refs_per_tagger` (1,000) references within
   the same days count.

With `collapse=group`, a group needs enough shared taggers among its members that pass the other
filters, together.

## Resolution and status

`POST /v1/resolve` queues a supported URI and waits up to three seconds for its card:

| Status | Answer |
| --- | --- |
| 200 | The card with the [read's fields](#resource-read) |
| 202 | The card with `status: "pending"`: queued. Poll `GET /v1/resources/{id}` with its `id`. `Retry-After` (seconds) is there when the next attempt is put off, such as a missing answer that waits for the fetch that confirms it; it is absent while the job is queued or running. |
| 400 | An unsupported scheme, a disabled resolver, or a [link that carries a credential](#links-that-carry-a-credential) |
| 404 | The resource expired while the request waited (`resource expired`) |
| 429 | Over a preview budget ([Preview budgets](#preview-budgets)) |
| 503 | The instance's new previews for this hour are taken, with `Retry-After`; nothing was stored |

Ingestion and every preview share one queue entry per normalized URI, with one TTL, retry
schedule and dead-letter state. A resource that was only previewed, never referenced, is not a
search result, and expires after `limits.preview_grace_hours` (a day).

### Preview budgets

Previews are limited per client address and per URI: `limits.preview_requests_per_minute` and
`limits.preview_uri_requests_per_minute`, 30 each. Over either, the answer is 429 with
`Retry-After: 60`. The client address is the connecting one, or behind a proxy in
`api.trusted_proxies` the one it forwards.

New links are limited too. The instance stores at most `limits.preview_new_per_hour` (1,000)
links it did not know in any hour, and one client address at most
`limits.preview_new_per_client_per_hour` (100; 0 is no share) of them. Over the first, a new
link gets 503; over the second, 429. Each carries `Retry-After` (seconds), and nothing is stored.
A known link still reads.

### Links that carry a credential

`POST /v1/resolve` refuses a link that carries a credential with 400. Such a link is not stored,
does not count against the preview budget, and its log line names the host only. A link carries a
credential when it has:

- a calendar provider's secret share link (the patterns `scripts/verify-no-capability-urls`
  keeps out of the repository);
- a password in the user info;
- a key such as `token`, `access_token`, `key`, `sig` or `signature`
  (`capability::CREDENTIAL_KEYS`), or one that ends like `client_secret`, `X-Api-Key`,
  `access_key`, `auth_token` or `session_id` once its case and everything but its ASCII letters
  and digits are set aside (`api key`, `user[password]` and `k-e-y` count too). A public list's
  page cursor such as `pageToken` does not count (`capability::credential_key`).

Such a key counts in the query (parameters separated by `&` or `;`), or with a value in a
form-encoded fragment or in any part of it after a `?`, `#` or `;` (a hash router's
`#/reset?token=…`). A plain anchor such as `#key` is not one. A link written scheme-relative
(`//host/p`) or as `webcal:` without `//` counts too.

A card someone's post made for such a link is still read by its URI. Elsewhere such a link is
kept out of everything Pinakes stores or sends:

- An answer whose fetch ends on such a link (a page, an ActivityPub object, an oEmbed answer) is
  read in that address's directory without its credential, or at its origin when the path is a
  known secret. So nothing it names relative to that address (an empty `og:image`, a JSON-LD
  `#image`) carries the credential.
- Nothing stored names such a link but the resource's own link as written: not a document, its
  change lines, its episodes, its derived links (a source's link that carries one is refused), a
  calendar's occurrences, the fetch state, or the OSM place cache and its display names. A UID or
  a feed item's ID that is such a link is kept by its hash, and a change line names the
  occurrence by that hash. Another spelling of the link is kept only when it adds no credential
  (no user info, no key in its fragment).
- It is never sent to another service: also-on lookups (the hubs, the Wayback Machine) and oEmbed
  providers skip it, and the MCP `social` tool leaves it out of what it asks Nexus.
- It is never kept as another link's alias.

### Statuses

| Status | Meaning |
| --- | --- |
| `ok` | The current metadata was accepted. |
| `pending` | Queued and not resolved yet. A link's first missing answer leaves it `pending` until a second fetch confirms it. A link with no accepted card yet (a new preview) is asked again after `limits.backoff_secs` (30 s), and `POST /v1/resolve` answers 202 with `Retry-After` until then. |
| `missing` | Confirmed gone by two fresh fetches. One missing answer keeps the last accepted metadata; the second fetch comes after `limits.missing_ttl_secs` (at least an hour). A repeated cached observation does not count twice. |
| `unreachable` | The upstream denied access (401, 403) or answered with a challenge page (a bot wall, a captcha or a sign-in redirect). It is retried with backoff and dead-letters after `limits.max_failures`, as an error does ([access walls](design.md#access-walls)). A denied secondary request (`upstream_denied`, such as an event platform's) is retried after the wait it names instead: the status turns `unreachable` only after three days, and the link never dead-letters. |
| `error` | The fetch failed. It is retried with backoff and dead-letters after `limits.max_failures`. |

A failed refresh keeps the last accepted metadata. Whether the status changes depends on the
failure ([failures and retries](operations.md#failures-and-retries)):

- Most failures are charged and set the status at once, on an accepted card too: `error`, or
  `unreachable` for a denial or a challenge page. This covers a web page, calendar feed or Pubky
  object that answers 5xx, 408 or another 4xx that is neither missing nor a denial. The link
  dead-letters after `limits.max_failures` failures in a row: at the defaults, about 8 minutes
  (30 + 60 + 120 + 240 seconds of backoff).
- A timeout is charged only past `limits.timeout_grace` in a row (by default from the fourth).
  From then on the status is `error`.
- Some answers are a wait, not a charged failure: an official source, an ISBN or DOI card or an
  event platform that answers 5xx, 408 or 429, Nominatim's 5xx, and a 429 on any other request.
  The link waits the longer of the answer's `Retry-After` and the backoff, which grows while the
  answers last. An accepted card keeps its status, and a link with no card yet shows `error`
  after `limits.max_failures` in a row. Such a link dead-letters, and turns `error`, only once
  the failures have lasted three days.
- A rate limit on the operator's own quota (GitHub's token, the operator's Nominatim) is no
  failure of the link.
- A request that never got an answer from the upstream (the connection refused, reset or
  aborted, or the host unreachable) says nothing about the link. An accepted card keeps its
  status, and a link with no card yet shows `error` after `limits.max_failures` in a row. It is
  retried with the capped backoff for as long as that lasts, and never dead-letters.

### Refresh

Refresh follows the time to an event's start ([refresh](design.md#refresh)):

| Time to start | Interval (`limits.refresh_resource_secs`) |
| --- | --- |
| More than 30 days, or no start | 7 days |
| 7–30 days | 2 days |
| 24 hours to 7 days | 6 hours |
| Under 24 hours, or running | 1 hour |

Calendars and feeds use `limits.refresh_container_secs` (1 day, 12 hours, 6 hours, 1 hour). A
feed's own TTL takes the place of the first two and caps the third. Cursor feeds refresh every
`limits.refresh_cursor_secs` (5 minutes). Every other interval is at least an hour. Adapter
floors can lengthen these; OSM follows `limits.osm_ttl_secs`.

One fetch at the end plus 24 hours (the start plus 24 hours without an end) makes a resource
final. A final resource stays frozen until a reference newer than its last resolution reopens
it. `geo:` points stay final.

## Aliases

An alias is the same object under another URI ([identity and aliases](design.md#identity-and-aliases)).
It keeps its own resource ID (an ID never changes), but it is never fetched:

- its references count for its root;
- search lists only the root;
- a read, lookup, change query or occurrence read of the alias answers with the root, so the
  card's `id` is the root's.

A spelling nobody referenced is not an alias yet. `by-uri` and the `uris` of
`POST /v1/resources/lookup` still answer it with its root when that root is stored, by the rules
below that need only the URI (not a page's canonical or an API's answer). Nothing is fetched or
stored for it.

Another scheme (`http` for `https`), `www.` or a trailing slash is another link unless someone
referenced that spelling.

Aliases follow these rules:

- **Tracking parameters**: `utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `igshid`, `mc_cid`,
  `mc_eid` and `_gl` on every host; `recId`, `recSource`, `searchId` and `eventOrigin` on
  meetup.com; and `tk` on luma.com and lu.ma. They are dropped from the raw query, and every
  other value keeps its encoding. A URI that had any is an alias of the URI without them.
- **OSM spellings and object shortlinks** are aliases of
  `https://www.openstreetmap.org/{type}/{id}` ([OpenStreetMap and locations](#openstreetmap-and-locations)).
- **Identifiers**: an ISBN-10 or hyphenated ISBN is an alias of `isbn:` and its ISBN-13.
  `https://doi.org/…`, `https://dx.doi.org/…` and any letter case are aliases of `doi:` in lower
  case.
- **Official-API spellings**: every address of an object a [source](#sources) reads is an alias
  of one root; for example, each Federal Register address of a document is an alias of
  `fr:{number}`. When the API itself names another root for the object, the link becomes an
  alias of that root after its first fetch.
- **Cursor feed variants** (BTC Map's delta feed with parameters, the portal's endpoints) are
  aliases of their feed.
- **Music service links**: every spelling of one track, album, artist or playlist on one service
  is an alias of its canonical URL ([Music service links](#music-service-links)).
- **Wikidata items**: `wikidata.org`, `m.wikidata.org`, `/entity/Q…` and
  `/wiki/Special:EntityPage/Q…` are aliases of `https://www.wikidata.org/wiki/{Q-id}`.
- **Wikipedia articles**: the mobile host, `index.php?title=…` (a revision with `oldid` too),
  spaces and percent-encoding are aliases of `https://{lang}.wikipedia.org/wiki/{Title}`. A
  redirect's title stays its own link.
- **MusicBrainz pages**: a recording, release, release group or artist page, or an ISRC's page,
  on `musicbrainz.org`, `www.` or `beta.`, over http or https, its web service record
  (`/ws/2/…`), the MBID in any letter case, a trailing slash and parameters are aliases of
  `https://musicbrainz.org/{kind}/{mbid}` or `https://musicbrainz.org/isrc/{ISRC}`.
- **The page's own canonical**: after a fetch, a page becomes an alias of the URL its head
  canonical link or all-permanent redirect chain names, when that URL differs from it only by a
  trailing slash and tracking parameters. A canonical to another path (a homepage, a sibling
  page), another host or another query (`/events/?event_id=42` naming `/events/`) never
  aliases.
- **A Meetup date's number and token** ([below](#meetup-dates)).
- **A podcast episode's page** ([below](#podcast-episode-pages)).

### Music service links

| Service | Spellings | Canonical URL |
| --- | --- | --- |
| Spotify | `intl-xx` paths, `spotify:{kind}:{id}`, the `user/…/playlist/…` form, `play.spotify.com`, the embedded player's `/embed/…` and `embed.spotify.com/?uri=spotify:…` | `https://open.spotify.com/{kind}/{id}` |
| Apple Music | Any storefront and name slug; an album URL with `?i={id}` names that track (`/song/{id}`); the iTunes Store's `itunes.apple.com/…/id{N}` | `https://music.apple.com/{kind}/{id}`, without the storefront: the catalogue ID is global, and Apple answers the storefront-less URL with the viewer's storefront |
| YouTube, YouTube Music | `youtu.be`, `m.` and `music.youtube.com`, `/shorts/`, `/embed/` | `https://www.youtube.com/watch?v={id}`, `…/playlist?list={id}` |
| Deezer | The language path | `https://www.deezer.com/{kind}/{id}` |
| TIDAL | `/browse/`, `listen.tidal.com`, the `/u` share suffix (tracks, albums, artists, videos, playlists) | `https://tidal.com/{kind}/{id}` |
| Qobuz | Store album pages, `play.qobuz.com` | `https://open.qobuz.com/{kind}/{id}` |
| SoundCloud | `m.`, any letter case, parameters | `https://soundcloud.com/{user}`, `…/{user}/{track}` or `…/{user}/sets/{set}` |
| Amazon Music | `albums/{id}?trackAsin=` names that track; Amazon's own hosts only: `music.amazon.com`, `.co.uk`, `.de`, `.fr`, `.it`, `.es`, `.ca`, `.com.au`, `.co.jp`, `.in`, `.com.br`, `.com.mx` | `https://music.amazon.{tld}/{kind}/{id}` |
| Bandcamp | Parameters | The URL without parameters |
| Discogs | The name after the ID, a language path, no `www` | `https://www.discogs.com/{kind}/{id}` |

Short links that need a request to expand (`spotify.link`, `deezer.page.link`,
`on.soundcloud.com`) are not folded.

### Meetup dates

Meetup serves each date's page under its number (`/{group}/events/316908442/`) and its token
(`/{group}/events/frxnbvyjcqbmb/`), and names the token in the page's head canonical. After a
fetch that ended at the page itself (no redirect to another page), a meetup.com event page
becomes an alias of the other spelling when its canonical names it: a number for a token, or a
token for a number, in the same group's `/events/` directory, on the same host, without a query.
Both spellings then answer with one card. A canonical to another number or token, another group,
another host, the group's calendar or any other path never aliases.

### Podcast episode pages

After a read of an episode's feed item (`{feed}?guid={guid}`, as [Episodes](#episodes) lists it)
that ended at that address (no redirect), the page the item links becomes an alias of the
episode's card, without its tracking parameters. All of these must hold:

- the page is the item's own page;
- the item is in a show's stored list;
- the feed's publisher publishes the page too: one registrable domain, or one tenant of a shared
  host, on one host or a subdomain of the other.

A blog post whose markup reads as an `Article` then answers as the `PodcastEpisode`, and its
tracked spellings with it.

What is never an episode's own page: a page another item links too, the feed's own `<link>` (the
show's site), a site's root, a feed and a feed's item.

Only a page Pinakes has read joins, and only while it is a plain page of its own: no group, no
aliases but its own tracked and slash spellings, read as a web page or an article, or as an
episode of the same feed. A page not read yet joins at the item's next read after its own. A page
that is already an alias, or that a rule above gives another root, is left as it is.

When reads of the item confirm it gone from its feed (the card `missing`), or a read finds it
linking another page or none of its own (a redirect too), the page it had answers for itself
again, with its tracked spellings, and is read again. A page on another site than its feed (a
show hosted elsewhere) keeps its own card.

The publisher test uses the registrable domain and the shared hosts Pinakes knows
(`data/multi-tenant-hosts.json`). On a host that serves several publishers by path and is not
listed there, one publisher's feed can still name another's page that reads as a plain page or
article.

## Changes

`POST /v1/changes/lookup` answers what changed for the links a client keeps.

| Body field | Default | Meaning |
| --- | --- | --- |
| `resource_ids` | None | Resource IDs |
| `uris` | None | Links, each read as `POST /v1/resources/lookup` reads it. An alias spelling is the alias: its lines name its root, and its changes include the root's. So the same `uris` list works for both lookups. |
| `since` | None | Inclusive: RFC 3339, Unix milliseconds (an integer or text) or relative (`now-30d`), within UTC years 0001–9999. Without it, every retained change of those resources, in pages. |
| `cursor` | 0 | The previous page's `next_cursor` (an integer) |
| `limit` | 100 | Lines per page, 1–500 |

`resource_ids` and `uris` take 1–500 entries together. Unknown IDs and links add nothing.

The answer is a page `{lines, next_cursor, has_more, oldest_cursor}` of
[change lines](#change-line), ascending by `seq`, at most `limit`:

- Continue with `next_cursor`. When you are caught up, it advances to the head even if nothing
  matched.
- A nonzero `cursor` older than the retained history gets **410**: start again without a cursor
  and refresh the resources you keep.
- A relative `since` is fixed by the first page. Later pages (with `cursor`) keep that window,
  capped at the cursor line's time less one second, so lines written between page fetches are
  not skipped.
- Prefer `cursor`. When you use `since`, reuse a previous line's `at` and deduplicate by `seq`.

Lines are kept `limits.change_retention_days` (90) and pruned hourly. History outlives orphan
expiry.

### Classes

| Class | Meaning |
| --- | --- |
| `created` | The resource or occurrence got its first document |
| `rescheduled` | Its start or end changed |
| `relocated` | Its location or venue changed |
| `cancelled`, `uncancelled` | It was cancelled or went `missing`, or that was undone |
| `renamed` | Its name changed |
| `details` | Anything else changed |
| `deleted` | Its document was removed; orphan expiry emits it |
| `aliased` | It became an [alias](#aliases) ([below](#aliased-lines)) |

When several fields change at once, one class is chosen in this order: lifecycle, time,
location, name, details. `fields` still lists them all. A fetch failure emits nothing and never
cancels a resource.

A calendar snapshot with more than `limits.feed_max_changes` (100) occurrence changes emits one
feed-level `details` line with `fields: ["occurrence_changes"]` (and `"resource_change"` when the
feed's own card changed too). Read the occurrence window again then.

### Aliased lines

A resource that becomes an alias emits `aliased` with `fields: ["root"]` and `root`, the ID that
answers for it from then on. When an alias answers for itself again (no rule names its root any
more, after an upgrade's `reextract`), `root` is its own ID. The changes of an alias's ID are its
own and its root's.

## Calendars and occurrences

ICS feeds, event platform feeds, BTC Map and NIP-52 calendars all produce the same occurrences
([events and calendars](design.md#events-and-calendars)).

### One feed

`GET /v1/resources/{id}/occurrences` returns a feed's accepted occurrences, ordered by start, UID
and recurrence key. Superseded rows are left out.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `from` | The start of the last accepted horizon | Inclusive lower bound on the start, RFC 3339. An earlier explicit bound reads retained older rows. |
| `to` | None | Inclusive upper bound on the start, RFC 3339 |
| `limit` | 100 | Occurrences per page, 1–500 |
| `cursor` | None | The previous page's `cursor` |

The signed `cursor` fixes the resource and window, and expires when the daemon restarts. A
tampered or mismatched cursor is a 400. A known resource that is not a calendar returns an empty
list; an unknown ID is a 404.

### Several feeds

`POST /v1/occurrences/lookup` reads several feeds' occurrences as one page, ordered by start,
then feed, UID and recurrence key. Each card's `id` names its feed.

The feeds are `resource_ids`, `uris` or both, 1–500 together. A link is read as
`POST /v1/resources/lookup` reads it. An alias reads its root, and an unknown ID or link adds
nothing.

The window, `limit` and `cursor` work as for one feed, but `from` defaults to the earliest
horizon start among the feeds. The cursor is bound to the resolved feeds (an alias as its root),
in any order and however they were named.

### Other dates of an event page

A page that has its own date (a Meetup date) is its own resource, and the calendar listing the
organizer's dates is another. Reading a Meetup date's page queues its group's calendar
(`/{group}/events/ical/`) as a resource.

The page and that date in the calendar are one [group](#groups). From the page's card,
`GET /v1/groups/{group.id}` lists the member whose `occurrence` names the calendar (`feed_id`,
`feed_uri`), and `GET /v1/resources/{feed_id}/occurrences` lists every date. A group calendar
mixes the group's series; its other dates are occurrences, never members of the page's group.

Meetup writes its descriptions in Markdown. On meetup.com a card's and its dates' descriptions
are read as plain text: emphasis, heading, quote and list marks are dropped, and links are kept
as their text.

### Times

- An offset-only timestamp keeps its instant.
- Floating times take the TZID, `X-WR-TIMEZONE`, `event.default_timezone` or the zone of the
  coordinates.
- Recurrence follows RFC 5545 across DST changes. An ambiguous wall time (the repeated autumn
  hour) occurs once, at its first instance; a nonexistent spring time moves forward by the gap.
- A series whose DTSTART is UTC (`…Z`) expands in UTC unless `event.utc_series_wall_clock`
  selects a zone source. The default, `x_wr_timezone`, takes an explicit `X-WR-TIMEZONE` (Google
  exports UTC DTSTARTs with a local rule) or a TZID. `coordinates` also takes the default and
  coordinate zones, and `never` keeps UTC.
- An UNTIL is compared as an instant. Floating RECURRENCE-ID, RDATE and EXDATE values resolve in
  the series' zone.
- A series whose embedded VTIMEZONE cannot convert its DTSTART expands in UTC with the warning
  `series_zone_unconvertible`.
- All-day ends are exclusive dates.

### Bounds

Event years have four digits (RFC 5545 `date-fullyear`). Stored instants are limited to UTC
years 0001–9999 and truncated to microseconds; a leap second rolls into the next second. An
occurrence starting outside that range is dropped (`occurrence_start_out_of_range`). An end or
modification time outside it is dropped from its item, so one bad item never fails a feed.

Recurrence expansion, components, revisions, lines, body size and parse time are capped by the
`event_*` limits ([operations](operations.md#calendars-and-events)).

A recurrence rule is expanded only when it repeats yearly, monthly, weekly, daily or hourly and
one of its periods holds at most `event_max_period_dates` (100,000) dates. A period's dates are
the days the period keeps after the rule's day filters, times its listed hours, minutes and
seconds. Any month-day list counts as the whole month (31 days), the rule's own or the one taken
from DTSTART, so a plain monthly or yearly rule counts 31.

- An event with another rule keeps DTSTART and its RDATEs, less its EXDATEs, within the
  horizon, with the warning `series_rule_unexpanded`.
- A VTIMEZONE with such a rule cannot convert, and an event in that zone is left out with a
  warning.
- An event with more than one RRULE, or with an EXRULE, is left out with a warning.

### Changes and disappearance

Rows are compared by SEQUENCE, LAST-MODIFIED and a normalized content hash, so DTSTAMP and fetch
noise create no changes.

- Explicit cancellations apply at once.
- Disappearance needs two fresh complete snapshots. Roll-off past the horizon, or past the end
  plus 24 hours, never cancels.
- A new UID with identical content supersedes the old row without a cancellation, also when the
  date's own page link changed with it (a publisher that renumbers a date, as Meetup does).
- The same holds when the new UID was listed first. A key that would be cancelled, or was, while
  another key lists the same date (same start, name and content but for its link, status and
  looked-up venue) is superseded when that key was first listed after the old one was last
  listed. Keys listed side by side for longer are separate sessions.
- A re-parse that moves instances (for example by an hour across a DST change) re-keys them
  once, under the same rules.

Pinakes holds a suspicious snapshot instead of applying it:

- A snapshot that loses more than half its active keys, or adds more than
  `limits.feed_max_new_keys` (1,000) to a feed with rows, is held until an identical snapshot
  confirms it.
- A snapshot that would take a feed's stored and new rows together over
  `limits.feed_max_rows` (10,000) is held until it shrinks. A snapshot that lists more than that
  on its own is not held: it fails like an error.
- `limits.feed_max_holds` (5) consecutive holds dead-letter the feed.

A feed answering 404, 410 or 451, or an empty feed, counts as an empty snapshot: two of them
cancel its rows and mark it `missing`, while its history stays readable here. A feed that
answers HTML instead is `unreachable` (retried with backoff) and keeps its rows. Cancelled,
superseded and past rows are purged `limits.feed_retention_days` (30) after their end.

Calendar validators (`ETag`, `Last-Modified`) are sent only after a saved snapshot with nothing
held or pending and more than half the forward window left; otherwise an unconditional GET
fetches a body. A 304 renews the TTL, but neither confirms a disappearance nor extends the
horizon. Mixed dated and undated RSS, Atom and JSON feeds keep their dated items.

### Pubky events

Social events and RSVPs (pubky-social-specs v1.1, `pubky://<author>/pub/social/v1/posts/{id}`)
are Nexus's: Nexus indexes their time, occurrences, RSVPs and counts, and a client reads them
there. Pinakes never fetches or indexes them. It indexes external events (pages, calendar feeds,
NIP-52), and a client merges both.

The resource feed carries no event lines: a `kind:event` line is skipped as `unknown_kind`, and a
`post` line for a social post is rejected.

### NIP-52 calendars

A calendar waits (`calendar_members_pending`) for members not yet resolved, at most
`limits.calendar_member_wait_hours` (48) after it first listed them. A member still unresolved
then, one whose resolution failed, or one without a start is skipped with
`calendar_member_unavailable`. The other members apply, and the skipped member's rows are
cancelled after two snapshots.

A calendar's link to a member it no longer lists ends at once. A listed member's link lives 24
hours, renewed at every fetch of the calendar. So a calendar that rotates its members never runs
into its cap, and the links of a calendar nobody fetches lapse.

## Episodes

`GET /v1/resources/{id}/episodes` lists a podcast show's latest episodes as its feed lists them,
newest first: the latest 1,000, in pages.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `limit` | 20 | Episodes per page, 1–20 |
| `offset` | 0 | Where the page starts, 0–1000 |
| `q` | None | Only the episodes whose `name` holds it, in any letter case (Unicode's, as Rust folds it: `über` finds `ÜBER`). At most 256 bytes; spaces around it are ignored, and a blank `q` matches every episode. |

The answer is `{items, next_offset, truncated}`:

| Field | Meaning |
| --- | --- |
| `items` | The page's episodes ([Episode](#episode)) |
| `next_offset` | The next page's `offset` while more episodes match, else `null` |
| `truncated` | Whether the feed lists older episodes than the list holds, as the show's last read found: that read ended before the feed did (past 8 MiB), or the feed has more than the 1,000 kept (exactly 1,000 is not). The same on every page, whatever `q`, so a client links the show on its own service for the rest. |

A known resource that is not a show, or whose feed was not read yet, returns an empty list. An
unknown ID is a 404.

A show's new episodes write no [change line](#changes). A client that shows the list reads it
again on a timer: when it shows the show again, or every few hours while it stays open.

```text
GET /v1/resources/{id}/episodes?limit=20
GET /v1/resources/{id}/episodes?offset=20&limit=20
GET /v1/resources/{id}/episodes?q=khabib
```

Episodes are a route and not a fact, because a fact is one string per name and a card's document
is bounded. They are not occurrences, because an episode has no instant to window by, no
recurrence and no document of its own here.

### Episode

Each item is `{uri, name, image, published, guid, stream, id?, aliases?}`. An entry is not a
card: nothing in it is tagged, grouped or counted.

| Field | Meaning |
| --- | --- |
| `uri` | The episode's feed item, `{feed}?guid={guid}` (the GUID URL-encoded), which `POST /v1/resolve` reads from the feed as the episode's `PodcastEpisode` card, whatever its page is (a blog post's markup may read as an `Article`). From a feed read at an address that carries a credential, `uri` is the item's page instead, and an item without a page is not listed. |
| `name` | The episode's title |
| `image` | The episode's image |
| `published` | When it was published (RFC 3339) |
| `guid` | What names the item: its GUID as the feed gives it; else its page; else `sha256:` and the hex digest of its enclosure's URL (without a fragment), so a link that carries a token shows nothing of itself |
| `stream` | The item's enclosure, when it is public and direct, in a card's [stream](#stream) shape with `preview: false`. `null` when the item has no enclosure, or a signed, expiring, credentialed or non-media one, or when its stored form would not fit the store's bound (a URL near 2,048 bytes that JSON escaping doubles); the episode is listed all the same. |
| `id` | The card the entry names, when Pinakes already has it (an alias as its root) |
| `aliases` | The entry's other links, at most 16; absent when there are none |

The feed item's card names its show in `related` (`rel: "show"`, the feed). The page the item
links, without its tracking parameters, becomes its alias when it is the item's own page on the
feed's own site ([Podcast episode pages](#podcast-episode-pages)).

#### Publisher pages

The publisher's pages of an entry are the cards on the feed's own site (status `ok`, not an
alias, by the rules of a page that joins) whose podcast links name this feed item by its item key
(`podcast:item:…`). That key is given once the show's GUID is vouched for and the feed names the
page as the item's link; a `maybe`, an expired or a switched-off source's link never counts. The
pages are read as a `PodcastEpisode`, or as a page that may join one.

When one of them is a `PodcastEpisode` card of its own, not joined to the item (a
[group](#groups) holds it, for example), the entry names it instead: `id` is that card (the one
Pinakes knew first) and `uri` its link. A directory's page of the item is never a publisher page:
neither a page on another site (Apple) nor a Podcast Index page, even for a feed Podcast Index
hosts.

`aliases` lists the entry's other links, which a client reads together with `uri` (tags, counts,
replies), in this order:

1. the feed item, when `uri` became another card's link;
2. the link of the card the feed item reads as;
3. the publisher's other pages;
4. the aliases of each of those cards, the named card's first.

`id`, `uri` and `aliases` are named at each read from what Pinakes knows then. The entries
themselves change only when the show is read again.

### On the card

Every card of a show (`PodcastSeries`, status `ok`) carries `episodes`: the first 5 entries of
this list that have a `stream` (an item without public audio or video is left out), newest
first. A client plays a show's latest episodes from its card with the player it uses for an
episode's `stream`, without a second request.

A show without a list of its own takes its group's, as the route does: the Apple Podcasts,
Spotify and Podcast Index cards of a show grouped with its feed carry the same episodes. The
entries are read from the stored list, never from the feed: a card read fetches nothing, and the
list changes when the show is read again (its card's refresh). A show with no such episode, and
every other card, has no `episodes`.

```json
"episodes": [
  {"uri": "https://feeds.megaphone.fm/darknetdiaries?guid=prx_7057_c0abbff3-ae98-4e23-a389-5ee0fd824f51",
   "name": "182: Blackshades",
   "image": "https://f.prxu.org/…/Ep182_BlackshadesSquare.jpg", "published": "2026-11-03T08:00:00Z",
   "guid": "prx_7057_c0abbff3-ae98-4e23-a389-5ee0fd824f51",
   "stream": {"url": "https://www.podtrac.com/pts/redirect.mp3/dovetail.prxu.org/…/darknet-diaries-ep182-blackshades.mp3",
              "type": "audio/mpeg", "duration_s": 2357, "preview": false}}
]
```

### How the list is read

The list is read with the show:

- The feed's own card (`PodcastSeries`) takes it from the feed it is.
- A Podcast Index or Apple Podcasts show page, and the player pages that open a show by its Apple
  ID, fetch the feed their record names, once per read of the show, and take the same list.

The list is replaced whole each time the show's card is saved from a read of its feed. A read
that could not reach the feed, or could not read and parse it within three seconds and what is
left of the read's own time, keeps the list it had: the list is optional, and a slow feed never
costs the show its card. A card replaced without a read (cleared after an error, scrubbed) lists
nothing.

A show card without a list of its own (a page whose feed could not be read, a service that names
no feed) answers with the list of a member of its [group](#groups) that has one: the same show
elsewhere. Of those, it takes the member whose newest episode is the newest, and of those the one
with the longest list. Only a card that is a show (`PodcastSeries`, status `ok`) borrows a
member's list. A card cleared after an error, found missing, or of another kind lists nothing,
whatever its group holds.

#### Long feeds

Only a feed's first 2,000 items are looked at, and the newest 1,000 of them kept: feeds list
newest first, and a few are out of order. A feed is read for its episodes up to 8 MiB, past the
1 MiB page cap, and cut at its last whole item there.

A feed card's own read is cut at the page cap, like any page's. When that read gives the show, or
when an episode's feed item asked for is past the part read, the feed is read again up to 8 MiB
at the show's feed address (the item's `?guid=` dropped, so a show's items share one read),
within three seconds and what is left of the read's own time. A cached copy serves the show card
when it begins as the cut read does (else the feed is read afresh, once), and serves an item card
as it is. The longer read is kept only when it gives the show, or that item's own episode.

The cut read's card and list stand when the longer read fails, is slow, gives neither, or does
not parse (a malformed byte or entity past the first megabyte). The cut list then keeps the other
episodes a longer stored list holds, so one slow read does not cut a show back to the first
megabyte's episodes:

- For a feed that lists its newest episode first: when the cut list holds the stored list's two
  newest episodes, each stored episode it lacks and no newer than its newest joins it, in date
  order, with whether that list's feed lists older ones. An older episode the feed has dropped
  stays listed until a read that is not cut replaces the list.
- For a feed that lists its oldest episode first (its dated items' dates ascend as read): the cut
  is at its oldest episodes instead. The cut list keeps every other episode the stored list holds,
  newer ones too, when it holds the stored list's two oldest dated episodes no older than its own
  oldest.
- When the stored list is full (1,000 episodes) and every one of them is newer than the cut's
  newest, the feed is longer than is kept and the cut is its old end. When the cut's feed names
  the same show as the stored card (the same title, or for a Podcast Index show the same
  website), the stored list stays as it is. A cut that names another show stands alone.

A Podcast Index or Apple show's feed that does not parse past the page cap is read within it, and
keeps a longer stored list's other episodes the same way.

A read that stops before its feed ends at 8 MiB, or with items past the ones looked at (the first
2,000, or a feed card's first `limits.podcast_items` when that is lower), holds a newest-first
feed's newest episodes and replaces the stored list, so an episode the feed has dropped goes. For
a feed that lists its oldest episode first it is a cut read (a feed card's, Podcast Index's and
Apple's alike), and keeps the newer episodes a longer stored list holds. A whole read is never a
cut one, however many episodes past the 1,000 kept it has. A page's alternate feed is read within
the page cap.

## Collections

A public collection is a Pubky post that lists links. Nexus indexes its items; a private
collection or a bookmark never reaches Nexus. Pinakes never fetches a collection: ask Nexus or
the author's homeserver for its name and items.

`GET /v1/resources/{id}/collections` lists the collection posts that hold the card's link: its
own `uri` or one of its aliases' (the 31 most recently collected, as [`refs`](#refs) bounds its
aliases). Each post is listed once, however many of the card's links it holds.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `viewer` | None | A public key (52 characters): that user's own collections come first, their 100 newest; any more are listed with the others |
| `limit` | 20 | Posts per page, 1–100 |
| `cursor` | None | The previous page's `cursor` |

The answer is `{items, cursor}`:

| Field | Meaning |
| --- | --- |
| `items[].uri` | The collection post (`pubky://<author>/pub/<namespace>/v1/posts/<id>`) |
| `items[].author` | Its author's public key |
| `items[].indexed_at` | When Nexus indexed the version that holds the link |
| `cursor` | The next page's cursor; `null` on the last page |

After the viewer's own, the posts are newest `indexed_at` first, then by author and post on a
tie. The cursor is signed, bound to this resource and `viewer`, under 6,000 characters however
long its paths (the route refuses any over 8,192), and invalid after a restart: start again on a
400. An alias ID reads its root's list; an unknown ID is a 404.

Every card counts its collections in `collections`, and `in_collection=<uri>` on
[search](#search) lists the cards a collection holds.

```text
GET /v1/resources/{id}/collections?viewer={public key}&limit=20
```

### How the list follows Nexus

The list follows Nexus's reference feed. Each item of a collection post is a `collection` line of
its own, and a `del` line drops exactly that item, for a removed item, a deleted post or an edit.
An edit's new version's items come as new lines, and an edit moves the collection up. A post's
reply and embeds are kept the same way, one line each.

A post is listed once, at its newest, under its one URI. Its version files
(`…/posts/<id>/<editId>.json`) are its versions. Any other file, including a path nested deeper
under a post or a file that is not JSON, is a collection of its own. Of a post's files, the
current one is the one Nexus indexed last; of files indexed in the same millisecond, the greatest
name in byte order (the specs' newest editId: fixed-width timestamp IDs). Its items are the
current file's alone, for the list, the count and `in_collection`.

Nexus stages a post's version as one batch: a `del` for each line it drops, then a `put` for each
item, all at one instant. Ingest keeps the post at what that batch leaves: a put of a collection
file gives the file's every item its newest instant, and drops the post's other files. So a
skipped or late line never leaves a post at two instants or under two files.

What ingest cannot recover from a skipped line: an item whose only put was skipped is missing,
and an item whose del was skipped stays listed, until the post's next edit. A collection holds at
most 100 items, the specs' cap. A new item past them is refused (`collection_items` in
`pinakes_ingest_rejected_total`), also when a skipped del left one of the 100 stale.

### The viewer's own collections

The viewer's pinned set is fixed by the first page and carried in its cursor: the window from the
instant of their newest post to their 100th newest, as that page found them. While a client
pages, no post moves between the viewer's own and the others. A collection the viewer adds or
edits later is newer than the window, and is listed with the others, by time.

An edit still moves a collection up (its new instant), so a client paging through a list that
changes meanwhile can see a post again or miss one, as with any newest-first list.

### Read bounds

Every read is bounded, however many collections hold a link, an author made or the viewer holds,
ties of one instant included:

- **A page** (any cursor) reads at most `limit` + 1 posts per link (the card's own and each of
  those aliases).
- **The others' walk** steps over the viewer's pinned window alone: the at most 100 posts of at
  most 100 items each that the first page found, and any reference Nexus appended to its feed
  later with an instant inside the window (an append recovered after a later one, or one in the
  same millisecond as their newest).
- **The viewer's pinned window** reads at most 100 posts per link on a first page.
- **The count** reads at most 1,000 posts per link.
- **`in_collection`** reads the one post's files.

Each step is one or more probes of an index, each a range in the index's order: newest first,
then author and file; the viewer's own by author, never past their pinned boundary; a post's
files newest first.

## Groups

Spellings of one object are [aliases](#aliases): one resource. A group joins resources that are
one thing, only on evidence that cannot be spoofed ([keys and groups](design.md#keys-and-groups)).
A group is a hint, never a merge: each member keeps its own card.

### What joins a group

- **An identifier and its landing page**: a DOI or ISBN card and a page its registry record
  names as the landing page, when that page cites the identifier in its head (`citation_doi`,
  `prism.doi`, `dc.identifier`, `bepress_citation_doi` or `citation_isbn` meta). Open Library's
  own edition page needs no citation, and joins only an ISBN card naming it; two cards naming one
  edition page join only through the indexed page. A doi.org URL is the DOI itself, never a
  landing page.
- **One ICS UID within one feed**, between dated instances that start within five minutes of
  each other. The UID is hashed into the key, so no UID text names another feed. An event page
  takes the UID of its own calendar file (one event, linked on the page's own origin and answered
  from the page's own URL origin). It joins the occurrence with that UID in the organizer
  calendar its platform names for it on the same origin when both start within 300 s, located or
  not and whatever their names. For a Meetup date's page that calendar is its group's
  `/events/ical/`, read only when Meetup itself answers it over https. A UID never joins two
  feeds, two publishers, or a page and any other calendar, whatever their places, starts and
  names: such events stay apart, linked at most as
  [possibly the same event](#possibly-the-same-event).
- **An exact key a source vouches for** ([Also on](#also-on)): two links a hub names as the same
  recording, release or Wikidata item by its own identifier (`mb:recording:{mbid}`,
  `wikidata:Q…`), or a service's own record of its own object (`own_ids`): the ISRC on a TIDAL
  track page, a DOI a repository issued itself, or the Wikidata item a registry's or museum's
  record names. Any other page's claims are never such a key.
- **Curation** on the admin listener ([Curation](#curation)).

`sameAs`, canonical and other URL claims, and other identifiers, never group.

### Group rules

- A group holds at most one value of each single-object namespace: MusicBrainz recording,
  release, release group and artist (`mb:…`), Wikidata item (`wikidata`), IMDb title (`imdb`),
  Open Library edition and work (`openlibrary:…`), podcast GUID and Apple podcast ID
  (`podcast:guid`, `podcast:apple`), OSM object (`osm`), DOI (`doi`), and the namespaces each
  source's page in [docs/sources](sources/README.md#also-on) calls single-object. A join that
  would bring a second value (a wrong ID on a hub) is refused, so one bad claim never chains two
  groups. The database remembers every such namespace a source registered
  (`single_object_namespace`), so a removed source's keys keep the rule until they expire.
- ISRCs, barcodes, ISBNs and other keys may be several.
- An event page's keys are never such evidence: a concert's performer key (`mb:artist:…`) names
  the artist, not another copy of the concert, so events group only with events.
- Two located members more than `limits.group_max_distance_m` (500 m) apart never merge
  automatically.
- A group holds at most `limits.group_max_members` members.
- Points never group, and singletons stay individual.
- A grouping failure backs off on its own and never blocks a save.

An automatic group's ID is stable once formed. It stays as members join; a merge keeps the ID
most members carry; after a split the larger part keeps it. A new group is named after its
smallest member, and never takes an ID an existing group has. Member IDs and references never
change.

Nothing about a group's members is combined or stored with it, so each keeps its own card,
references, score and ranking. The group tables hold only membership, and no OSM-derived value
can reach them.

`groups.enabled = false` turns off group reads, writes and `collapse`: `GET /v1/groups/{id}` and
`collapse=group` get 400. Stored membership stays for when groups are turned back on.

### Music groups

Exact music membership is symmetric and transitive: a keyed A→B→C chain shows the other members
from every entry point. A music resource read includes the live exact keys of same-type members
(at most 256 members and 256 additional keys). Expired, disabled-source and `maybe` evidence is
left out.

A music card without its own image can reuse a same-type member's Commons or Cover Art Archive
image, together with its image credit. This fallback is applied when the card is read, not copied
into the stored document, and disappears when membership changes.

A recording's release is containment, not another copy of the recording. The
`musicbrainz_containment` source stores at most eight exact release URLs as
`facts.track_of_album`, never as grouping keys or also-on entries. It reads one bounded
MusicBrainz release page and retries incomplete reads.

MusicBrainz recording and ISRC pages can also use their verified ISRCs for exact reverse lookups
on other services; release pages use their UPCs. A service's answer must return the requested
identifier. Album links and UPCs never become a recording's identity.

### Curation

On the admin listener, `PUT /v1/groups/{id}` replaces a group's member set and answers as
`GET /v1/groups/{id}`:

```http
PUT /v1/groups/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Authorization: Bearer <admin-token>
Content-Type: application/json

{"members":[{"resource_id":"0123456789abcdef0123456789abcdef","occurrence":null}]}
```

An occurrence member is `{"uid":"calendar-uid","recurrence_id":"2026-10-20T17:00:00Z"}`.
Members must exist and have been grouped once.

Curation pins its members away from automatic groups, and overrides the distance and evidence
rules. Removing a member lets its automatic evidence count again. Invalid curation gets 400; a
busy group writer gets 503 (retry).

`DELETE /v1/groups/{id}` undoes a curation: its members are no longer pinned and the group is no
longer curated, so each member groups again by what its keys say. The group goes when no member
joins it again. It answers 204; 404 when the ID is no curated group; 400 for an invalid ID; 503
while the group writer is busy.

## Possibly the same event

One meetup is often listed on several sites (Meetup, Luma, its organizer's page). These pages
share no identifier, so they never [group](#groups). Instead an event's card names other events
that are possibly the same one in `maybe_same` ([maybe same](design.md#maybe-same)):
`{id, uri, site, start, score}`, best first, up to 3 on a search, lookup or group card and 10 on
a resource read.

An entry is an event resource, or one occurrence of a feed. For an occurrence, `id` is the feed's
ID and `uri` the occurrence's own page, else the feed's. A feed read from a service's API is named
by that service's page, never the API endpoint.

Two events are possibly the same when all of these hold:

- They start within 300 seconds of each other. When either is an all-day event, they start on the
  same local date, each in its own zone.
- When both are located, they are within 500 m of each other. When one or both have no
  coordinates, they name the same venue (its resource or OSM object) or the same organizer URL.
  So one organizer's meetups in two cities are not the same when both are located, but an event
  without coordinates pairs with the organizer's other event at that time and under a similar
  name, wherever that one is.
- Their names are at least 0.5 alike (pg_trgm similarity) once both are normalized:
  - lower case;
  - without dates, times and years (`21.10.2026`, `2026-10-21`, `6.11.`, `21.`, `19:00`, `7pm`,
    `2026`, a month name with its day such as `Oct 21` or `6th November`, in English, German,
    French and Italian);
  - without punctuation, symbols and emoji;
  - without weekday names and filler words (`meetup`, `event`, `edition`, `session`, `monthly`,
    `weekly`, `stammtisch`, `treffen`, `rencontre`, `incontro`).

  A name with nothing left never matches.
- Other numbers stay in the name (`#50`, `Nr. 30`, `Room 101`). When both names keep numbers,
  they are the same numbers: "Meetup #50" and "Meetup #51" are not the same event.
- They are on different pages: two occurrences of one feed never match.

An event without coordinates, a venue or an organizer URL is therefore never listed. An example
is a calendar entry without `GEO` whose `LOCATION` is one line of text (only a structured street
address is geocoded).

`score` is the name similarity, from 0.5 to 1. A member of the card's own group, an alias and a
gone occurrence are never listed. The hint is weaker than a group: nothing is merged, counted or
ranked by it, and each event keeps its own card.

A background pass computes the pairs for events starting from a day ago to a year ahead, and
again whenever an event's start, place or name changes; a request only reads them.

- Each event keeps its best 20 pairs and is named by at most `limits.same_event_incoming_kept`
  others (40 by default), so a change marks a bounded number of events to refill their lists.
- A read examines at most 100 candidates. One that runs out with room left goes on from where it
  stopped in a later read.
- A new or changed event is paired within about five seconds of its save, once the events before
  it in the queue are read.
- An hourly sweep, which also runs at startup, queues events that entered the window or changed
  unseen.

Search pages change their `ETag` when a hint on them changes. `[event] same_event_hint = "off"`
([operations](operations.md#calendars-and-events)) removes the field from event cards and stops
the pass. With the recording hint off too, it also removes the field from the OpenAPI document.

## Possibly the same recording

One recording is often on a page nothing exact joins to the others: a re-release whose ISRC
MusicBrainz does not know, or a service page that states no identifier. Such pages never
[group](#groups), because a group needs a key a hub or the service vouches for. Instead a
`MusicRecording` card names other recordings that are possibly the same one in `maybe_same`:
`{id, uri, site, start, score}` as for events, with `start` `null`, best first, up to 3 on a card
and 10 on a read (`GET /v1/resources/{id}`).

Two recordings are possibly the same when all of these hold:

- Their titles are equal once both are normalized:
  - lower case, `&` as `and`, punctuation as space;
  - without the tags that keep a recording's identity, in brackets or after ` - `
    ("Remastered 2011", "2011 Remaster", "Explicit", "Album Version", "Deluxe", "Bonus Track",
    "From the X Soundtrack", a `feat.` or `with` credit);
  - with the tags that name another recording ("Live", "Remix", "Radio Edit", "Acoustic",
    "Instrumental", "Demo", "Extended", "Mono", "Stereo", "Part 2", "Taylor's Version").

  So "Bohemian Rhapsody - Remastered 2011" is "Bohemian Rhapsody", and "Bohemian Rhapsody (Live
  Aid)" is not.
- Their first credited artists are equal, normalized the same way: the artist fact the
  [also-on](#also-on) sources read, else the one the page states. A recording without one names
  nothing.
- Their lengths, when both are known (the `duration` fact, else the page's), are within 3 s.

`score` is 1 when both lengths are known and agree, 0.7 when either or both are unknown.
Recordings whose artist and length (when they have one) a source states as facts come first, then
those whose own page alone states them, each by score. Any page can copy a track's title, artist and length into
its markup, and that never puts it ahead of a recording a service vouches for.

A member of the card's own group, an alias and a card that is not `ok` are never listed, so a
pair that a key later joins drops out of the hint by itself. The hint is weaker than a group: nothing is merged, counted or
ranked by it, and each recording keeps its own card.

A background pass computes the pairs, and again whenever a recording's name, type, artist or
length changes (its page or the sources' facts); a request only reads them. Each recording keeps
its best 20 pairs and is named by at most 40 others. A read examines at most 100 candidates, and
one that runs out with room left goes on from where it stopped in a later read. An hourly sweep,
which also runs at startup, reads the recordings the pass has not read (one saved while the hint
was off).

Search pages change their `ETag` when a hint on them changes. `[groups] same_recording_hint =
"off"` ([operations](operations.md#groups)) removes the field from recording cards and stops the
pass.

## Possibly the same app

An app's pages on different stores join one group when a Wikidata item names them (the `apps`
lookup family). Without such an item, an app's store card (App Store, Google Play, F-Droid,
Flathub, Snap Store) names the other store cards that are possibly the same app in `maybe_same`:
`{id, uri, site, start, score}` by URI, with `start` `null`, up to 3 on a card and 10 on a
resource read.

Two store cards are possibly the same app when:

- their official websites have the same host (without `www.`), and
- their names are the same once both are in lower case, with every run of characters other than
  letters and digits read as one space ("Signal - Private Messenger" and "Signal Private
  Messenger").

The pair is the `app_match` fact the apps family records for each card: the host and the name
(`signal.org signal private messenger`). `score` is 1. A member of the card's own group, an alias
and a missing card are never listed. Like the event hint, it merges, counts and ranks nothing,
and `[event] same_event_hint = "off"` removes it too.

## Also on

A card's `also_on` says where else the same thing is. It lists, in this order:

1. the other members of its [group](#groups): links someone tagged, each with its resource `id`,
   by ID and URI;
2. **derived links**: the same thing on services nobody tagged, found by a source, ordered by
   service name.

Each entry is `{id?, uri, refs?, site, derived?, via?, kind?, at?, embed}`:

| Field | Meaning |
| --- | --- |
| `id` | A tagged member's resource ID; absent on a derived link |
| `uri` | The link |
| `refs` | An exact, non-derived member's referenced links ([Member refs](#member-refs)) |
| `site` | The service: a member's site as its card names it, a derived link's service name |
| `derived` | `true` on a link nobody tagged, found by a source; absent otherwise |
| `via` | For a derived link: the hub or service it came through (`musicbrainz`, `wikidata`) |
| `kind` | For a derived link, a qualifier its source's page names: `maybe` (a match by metadata, not an exact ID: [Exact links and matches](#exact-links-and-matches)), `archived` (a Wayback Machine copy), `work`, `source`, `submitted`, `published` or `pdf` |
| `at` | For a derived link, a time that belongs to it, such as an archived copy's capture time |
| `embed` | The frame that shows this link ([Embed](#embed)), or `null` |

Each URL appears once (a tagged member wins), and never the card itself. A derived link is a
hint: it is never fetched, never counted as a reference and never ranks anything. It can be
unavailable in some countries (a regional release on a streaming service), and nothing public
says where, so prefer a tagged member.

### One link per service

Every card, a resource read too, carries one link per service: the service a link's URL is on (a
derived link's `site`). A search, lookup or group card lists at most 8 entries, a resource read
256.

Of a recording's or an album's links, the `editions` source places each
on a MusicBrainz release. A service shows its link on the card's own release, else on the
original edition, whether tagged or derived. The original edition is the first album that is no
compilation: its earliest release with a page on that service, never a deluxe, anniversary,
remastered or hi-res edition.

Among links it places on neither, the order is:

1. a tagged member, because someone used that link (of several, the one on the recording's
   original album, the `original_album` fact);
2. the derived link most group members name;
3. a page before a file (a `.pdf`);
4. the shorter link.

A link its service puts on another release (a later edition, a compilation) comes last.

The card's own service, by its URL, shows nothing else. An artist's card never shows another
page of its service, such as the band's earlier name, which Wikidata may list as a second ID.

How tagged members count against derived links:

- A music member whose card is not read yet counts by the service its URL names, against derived
  links only. It keeps derived links on that service off the card, is not deduplicated against
  other tagged members (two such members on one service both show until their cards are read),
  and counts within the entry cap.
- Any other tagged member whose URL is on a service a derived link names takes that service's
  place the same way (a tagged `f-droid.org` page is on F-Droid, as Wikidata's F-Droid link is).
  A page that only names itself after a service does not, and other tagged members always show.

A card of a service the sources know is named after it (`site`): Wikidata, OpenStreetMap, DOI
and the services each source's page names (the ones its derived links name). Only an object's
own page is on the service, in the forms its source recognizes (its page says which). Any other
page on those hosts (a topic list, a search, a user's page, a page under an object) takes no
service's place; its card keeps the name it gives itself, or its host.

### Member refs

An exact, non-derived group member can also carry `refs`: the same referenced links as its own
card, with the same selection and order as the card's [`refs`](#refs). At most 5 are listed, from
the canonical link and the 31 most recently referenced aliases, counts capped at 1,000, ties by
URI. The field is omitted when empty or equal to `[uri]`.

To join a group's tags, batch each member's `uri` and `refs` with the card's own links in Nexus;
no extra group read is needed for these members. Derived links and `kind: "maybe"` entries never
carry `refs`. These bounded lists are not every group member or alias. Gone social posts keep the
same alias-privacy rule as their own cards.

All cards and exact members in a response share one reference query, with member IDs
deduplicated before it; there is no query per entry.

### The sources

Sources read a resource after it is saved, and again when their results age (each source's
lifetime: 30 days, `btcmap` 7). A source's links and keys live twice that, so one failed refresh
keeps them, and they go when the source stops returning them.

The sources run in this order, each seeing the keys the ones before it found: `service`,
`tidal`, `spotify`, `musicbrainz`, `musicbrainz_entity`, `musicbrainz_containment`,
`performers`, `wikidata`, `discogs`, `deezer`, `spotify_isrc`, `tidal_isrc`, `itunes`,
`itunes_song`, `youtube_match`, `wikipedia`, `wikidata_item`, `books`, `films`, `tmdb`,
`podcasts`, `product_keys`, `own_ids`, `open_food_facts`, `stack_exchange`, `titles`,
`federal_register`, `mempool`, `osm_wikidata`, `wikidata_osm`, `wikidata_places`, `btcmap`,
`unpaywall`, `ncbi`, `datacite`, `huggingface`, `crates`, `npm`, `pypi`, `software_heritage`,
`steam`, `wikidata_stores`, `app_stores`, `wayback`, `editions`.
`[also_on.sources.<id>] enabled = false` turns one off. Each source's page in
[docs/sources](sources/README.md#also-on) says what it reads and finds and what it costs; the
index lists them in the order they run.

A MusicBrainz or Wikidata answer over `limits.hub_body_bytes` (8 MiB by default) is no answer.
The source then records only what the link's URL proves (a MusicBrainz page's MBID or ISRC, a
Wikidata item link's Q-id, the Q-id an article's own Wikipedia names), else nothing, until its
next run.

### Keys

A source's exact keys (an MBID, a Q-id) also [group](#groups) the links that share them. A key
comes from a hub (MusicBrainz, Wikidata) or from a known music service's own record of its own
object. Another host's claim (its JSON-LD `isrcCode`, `sameAs`, a canonical link) is a fact at
most.

A derived link found through an exact ID carries that ID: a MusicBrainz relation, a Wikidata
item's claim or sitelink, a service's answer to a lookup by barcode or ISRC. A tagged resource
that is that link (the same URL after its aliases) takes the ID too and joins the group, even when
its own lookup finds nothing.

A derived link is stored under its root spelling ([Aliases](#aliases): without tracking
parameters; an Apple Music link without its storefront). So one object found through two
spellings is one link, kept by the source that found it first.

A music link (a track, album or artist on a music service, in [its spellings](#music-service-links))
is looked up exactly, never by title or name, except by the sources that say they match
([below](#exact-links-and-matches)).

An ISRC that MusicBrainz gives to several recordings that agree names one recording not yet
merged. Recordings agree when they have the same artists, the same title once its
identity-keeping tags go, the same disambiguation, all audio or all video, and lengths within
3 s, the whole set alike. The ISRC and the MBID that stands for them (the smallest) are then the
track's keys, and the other MBIDs are facts. Recordings that disagree (a live version, a remix,
an edit, another artist) leave the duration to pick one, or the ISRC a fact
([an ISRC on several recordings](sources/README.md#an-isrc-on-several-recordings)).

A track with an ISRC key on another service is looked up on Spotify and TIDAL by it, and an album
with a barcode key by that, exactly (`spotify_isrc` and `tidal_isrc`, with the operator's
credentials). The service's own track or album that carries the value becomes a link keyed by
it.

ISRCs are stored in upper case and barcodes as GTIN-13 (a 12-digit UPC gains a leading zero), by
every source. The level is exact: a track lists tracks and videos of the recording, an album
lists albums and YouTube's album playlists, and an artist lists artist pages and channels. Album
links group by their release group, while two editions (two MusicBrainz releases) stay apart.

Apple Music links carry no storefront. Apple Books and Apple Podcasts links, and the iTunes
lookups, use the storefront `[also_on] apple_storefront` (`us`). MusicBrainz stores an Apple
Music link under the storefront an editor browsed, so a hub lookup of one asks for every common
storefront's spelling in one request.

### Exact links and matches

A derived link is exact when an identifier the service owns, or a hub, names it: a MusicBrainz
relation, a Wikidata claim, or a service's own answer to a lookup by ISRC or barcode (`deezer`,
`spotify_isrc`, `tidal_isrc`). It carries that key, so a tagged resource that is the link joins
the group; its entry is a plain derived link.

Three sources match by metadata, or by an answer they cannot read back, instead. Their links are
maybes, marked `kind: "maybe"` (possibly the same thing). They are still listed, so a client can
show or play them, but they are never keyed, never join a group and never make a key of the card
they are on.

| Source | Match |
| --- | --- |
| `itunes` (albums) | The one Apple Music album iTunes answers for the album's barcode; its answer carries no barcode to compare |
| `itunes_song` | Apple Music's song at the same disc and track number on the album with the track's barcode, with the same title and within 3 s of its length; iTunes answers carry no ISRC |
| `youtube_match` | For a label's YouTube upload (a Topic channel, "Provided to YouTube by"): the one Deezer and the one TIDAL track with the same artist and title within 2 s of its length |

### Wikipedia and MusicBrainz cards

An article's card comes from its Wikipedia's REST summary: `Article`, the title, the first
paragraph up to 600 characters, the thumbnail, and `site` `Wikipedia ({lang})`. So a long
article whose page exceeds the page cap still gets a card. A page the summary does not cover (a
special page) is read as a web page.

A MusicBrainz page's card ([its spellings](#aliases)) comes from MusicBrainz's web service (the
entity it names), not from the page. When that answer is over `limits.hub_body_bytes` (8 MiB by
default), the page is read as any page, and the sources record only what its URL proves, the MBID
or the ISRC. The card has:

- the title;
- the type: `MusicRecording`, `MusicAlbum`, `MusicGroup` or `Person` (an artist MusicBrainz calls
  a person);
- the artist credit and a recording's length as its description. An enriched description
  ([below](#musicbrainz-enrichment)) takes the place of this line. The resource read's `facts`
  still give them as `artist` and `duration` (in seconds): from the card itself as soon as it is
  read, and from the `musicbrainz_entity` source;
- `site` `MusicBrainz`.

An ISRC's page is its one recording's card. Of agreeing duplicates, it is the one with the
plainest title; that card states no `duration` of its own, and the `musicbrainz_entity` source's
duration is that of the recording that stands for them. An ISRC with no such recording gets a card
named `ISRC {code}`.

#### MusicBrainz facts

The resource read's `facts` of a MusicBrainz page also give what MusicBrainz says about the
entity in the same answer, with no further request, with or without enrichment. Each value is
whole and at most 256 bytes, else it is left out.

| Fact | Of | Value |
| --- | --- | --- |
| `disambiguation` | Any | MusicBrainz's comment that tells it from others of the name (`frenchcore artist`) |
| `artist_type` | Artist | `Person`, `Group`, `Orchestra`, `Choir`, `Character` or `Other` |
| `country_code` | Artist | ISO 3166-1 code (`NL`). The fact named `country` is a place's name elsewhere (Discogs' `Europe`), so a code has this name, as other sources' codes do. |
| `area` | Artist | The area's name (`Netherlands`) |
| `begin_date`, `end_date` | Artist | Its life span, as MusicBrainz gives it: `YYYY`, `YYYY-MM` or `YYYY-MM-DD` |
| `release_type` | Release group | Its primary type (`Album`, `Single`, `EP`, …) |
| `first_release_date` | Release group, recording | As `begin_date` |
| `genre` | Any | Its top genres: at most three, most votes first, ties by name, joined with `; ` |
| `rating`, `rating_count` | Release group, recording, artist | Its rating on MusicBrainz, 0 to 5 with one decimal (`4.5`; half rounds up), and the number of votes; neither when nobody voted. A release has no rating of its own. |

#### MusicBrainz enrichment

A MusicBrainz artist, release group, release or recording card also takes an image and a
description from the entity's Wikidata item, each with its [credit](#credits)
(`[web] music_enrichment`, on by default). So does an ISRC's page that shows a recording's card
(its one recording's, or of agreeing duplicates the one with the plainest title); the
`ISRC {code}` card takes nothing.

**Which item.**

1. The item the entity's own `wikidata` relation on MusicBrainz names. Several relations give
   nothing.
2. Without such a relation, the one item that carries the entity's MBID (an artist's `P434`, a
   release group's `P436`, a release's `P5813`, a recording's `P4404`), as Wikidata's search
   proposes and the item itself states at its best rank. Two such items, or more hits than a
   search returns, give nothing.
3. For an artist no item carries the MBID of: the item the artist's Discogs artist pages on
   MusicBrainz name (`P1953`, at most two), when every page that names one names the same item
   alone.

Nothing is matched by name. However it was found, by the relation too, the item is read back and
must be the entity's own. Otherwise the card takes nothing from it, and no other item is looked
for:

- It is never a Wikimedia page's item: `P31` a disambiguation page (`Q4167410`, `Q22808320`), a
  list article (`Q13406463`), a category (`Q4167836`, `Q15407973`) or a template (`Q11266439`).
- It is of the entity's kind, told by the identifiers it carries:

  | Item | Identifiers |
  | --- | --- |
  | An artist's | `P434` or a Discogs artist `P1953`, nothing else |
  | A release group's | `P436` or a Discogs master `P1954`. With a work ID `P435` or a song or single class (`P31`), a work's too: a single's song's, a rock opera's. |
  | A release's | `P5813`, no release group's |
  | A recording's | `P4404`, of no song class |
  | A work's | `P435`; or a song class with recording IDs and no work ID yet |

  An artist's ID with any other is no one's.
- An artist takes an artist's item; a release group a release group's, also one that is a work's
  too; a release a release's, never its release group's (an album's item often lists its
  editions); a recording a recording's that names that one recording, never a song's. Each names
  no other entity of its level.
- An item found by the MBID names the entity. One its own relation names may not name it back
  yet, and may carry no identifier at all yet (Wikidata lags behind MusicBrainz), except a
  recording's.
- An item too large to read is not verified and gives nothing.

A recording also uses the one work it is a performance of, for the description alone, when that
work has its own relation to an item that, read back, is the work's by the same rule: a work's
item naming no other work, a single's, or one with no identifier yet; never an album's, a
recording's, a release's, an artist's or a Wikimedia page's. Its credit names the work as
`subject`. A recording whose own item gives nothing uses its work's the same way.

**The description** is the first of:

1. the summary of the item's English Wikipedia article (plain text, at most 2,048 bytes, cut at
   a sentence or a word);
2. the item's English description on Wikidata, as it is (CC0, credited `wikidata`);
3. the summary of the item's article in the first of these Wikipedias it has one in: `de`, `fr`,
   `es`, `it`, `nl`, `pt`, `sv`, `pl`, `ja`, `ru` (credited with that `lang`).

For a recording the order is: its item's English article; its work's English article; its
item's English description; its work's; one other Wikipedia's summary, its item's first, else
its work's. A song's article tells a reader more than a one-line description of one recording of
it.

Cards have no language negotiation, and clients read English. An English paragraph is best; a
short, exact English line beats a longer paragraph a reader may not be able to read; another
language is better than nothing, and its credit's `lang` lets a client label or hide it. Nothing
is translated.

An item whose English article (or, read for want of an English text, its other article) is a
disambiguation page is no entity's. The card takes nothing of it: no description, not even its
English one, no image and no other Wikipedia. A recording still takes its work's text, and a
work's such item gives no text. A sitelink to a redirect gives no summary, because the page it
leads to is about something else. The summary's thumbnail is never used.

**The image** is the item's image (`P18`) on Wikimedia Commons, as a thumbnail asked 640 pixels
wide (Commons serves its next standard size, 960 pixels). It is shown only when the file's own
page gives a licence on the list (CC0, the public domain, CC BY or CC BY-SA 1.0 to 4.0, the GNU
GPL, LGPL or AGPL, Commons' `Attribution`) and, under every licence but CC0 and the public
domain, its author.

**The cover.** A release or release group with no such file (album covers are seldom free) shows
the front cover the [Cover Art Archive](https://coverartarchive.org/) holds for its MBID:

1. Pinakes asks once:
   `GET https://coverartarchive.org/{release|release-group}/{mbid}/front-500`, keeping the
   redirect, not following it.
2. The archive answers with a redirect to the file of the release the cover is of
   (`https://archive.org/download/mbid-{release}/mbid-{release}-{id}_thumb500.jpg`, read only in
   that form; for a release, its own file only). Any other answer is an unexpected one (below).
3. The image is that release's 500-pixel thumbnail on the archive
   (`https://coverartarchive.org/release/{release}/front-500`), credited `cover_art_archive` to
   the release's cover art page on MusicBrainz, with no author or licence.

A release is asked for only when MusicBrainz says the archive shows a front cover for it
(`cover-art-archive`: `front`, not `darkened`); a release group always (its cover is one of its
releases', chosen by the archive). A 404 is no cover. The archive is asked at the same time as
Wikidata, so a busy Wikidata does not leave an album without its cover. A free Commons file
always wins. When the Commons read failed or ran out of time, the cover shows only on a card that
has no stored image, and a stored image stays.

**Failures.** What enrichment does not find leaves the card as MusicBrainz makes it: an artist
without an item has no image or description, and the card is still `ok`.

- An answer that fails for a while (a timeout, a 429, a 5xx, Wikidata's `maxlag`, another
  unexpected status) keeps what the card had of that field. Should the card then pass its size
  bound, it gives up its last facts first. The card is read again after at least an hour and at
  least twice as long as since its last reading, up to its own refresh time, so a failure on a
  routine refresh waits for the next.
- A definite answer removes the field: no item, no text, a 404 or 410 from Wikidata,
  MusicBrainz, Wikipedia or the Cover Art Archive, or a refused file.
- A failed text never gives way to the next one in the order: a busy English Wikipedia keeps the
  stored article's summary rather than show the shorter line.
- A failed search for the item leaves both fields as stored.

## Cards and resolvers

Public `/v1/info` names the kinds this instance resolves in `resolves`. The admin `/v1/info`
lists one manifest per resolver and card kind (`media`, `isbn`, `doi`, `nostr`, `activitypub`,
`official`), the event platform adapters, and each resolver's switch, concurrency, request rate
and job counts by status.

| Kind | What it reads |
| --- | --- |
| ISBN | ISBN-10 and ISBN-13 with checksums; metadata from Open Library |
| DOI | `doi:`, `https://doi.org/…` and `https://dx.doi.org/…` request CSL JSON with the publication date and container. Invalid bare identifiers are final `missing`; informational DOI URLs fall back to the web route. |
| Nostr | Profiles and notes through the signed-event indexer. NIP-52 events keep their event schema; unknown signed kinds give a generic card. Relays (`event.adapters.websocket`) are asked only after both HTTP indexers miss. |
| ActivityPub | Advertised endpoints or ActivityStreams JSON give `Person`, `Article` or `SocialMediaPosting` cards, only when the fetched URL and the object ID share a registrable domain (on a multi-tenant host, the same tenant). Note and Article excerpts are capped at 200 characters; unlisted content is left out. |
| Feeds | [Below](#feeds) |
| Media | [Below](#media) |
| Event platforms | [Below](#event-platforms) |
| Official APIs | The [sources](#sources) |

Disabled identifier routes wait in the queue. An enabled web resolver still gives ordinary HTTP
links a card.

### Feeds

On every route, a feed is a `DataFeed`, named by its own title and described by its own
description or subtitle. Pinakes reads RSS, RSS 1.0 (`<rdf:RDF>` with its channel), Atom
(`<feed>` in the Atom namespace) and JSON Feed (a `version` on jsonfeed.org), whatever the
content type says.

- Only a field the format makes HTML loses its markup (an Atom `type="html"` text, an RSS
  description). Any other field keeps text that looks like markup as it is
  (`Using <title> in HTML`).
- A body served as `application/rss+xml`, `application/atom+xml` or `application/feed+json` that
  does not read as a feed is a `DataFeed` with no name.
- A podcast feed is typed as media ([below](#media)).
- A link that looks like a feed (its last segment ends `.atom`, `.rss`, `.rdf` or `.xml`, or is
  named `feed`, `rss` or `atom` with any extension or none, such as `feed.json`) is fetched with
  the feed types first in its `Accept` header, because some sites answer a feed's URL with a 404
  when HTML leads.

### Media

Media cards come from oEmbed (`media.oembed`) and the page's JSON-LD. The JSON-LD wins when it is
healthy, except over a provider whose scheme names the link exactly (YouTube's `watch` and
`shorts`), or any matching provider under `"always"`: their title, author and thumbnail come
first. A blank field in an answer is no value, so the page's stays. An oEmbed answer survives a
failed or truncated page.

- Podcast types need a namespace or an audio or video enclosure; any other feed is a `DataFeed`.
- A direct podcast feed is a `PodcastSeries`. `?guid=` or the episode page's RSS link selects an
  episode.
- A feed card longer than the page cap is read again up to 8 MiB for its show's episodes, or for
  an episode past the first megabyte ([Episodes](#episodes)), and past that up to its last whole
  item there (the newest come first). So is a feed of more items than `limits.podcast_items`
  (2,000), so an episode past that is never marked missing.
- No embed HTML or media bytes are served.

### Event platforms

The adapters are in `data/event-adapters.json`. Each has an `enabled` switch and, where the
platform needs one, a `key`. A missing key disables only that adapter, and no key is ever
returned. Its manifest lists the mode, cadence floor, identity, cancellation rule, expected
fields and canary.

BTC Map websites, NIP-52 calendar members and the group calendar of a Meetup date's page
([Other dates of an event page](#other-dates-of-an-event-page)) are queued as their own
resources (`event.adapters.derive_resources`).

A Wikidata item's card comes from the `wikidata` adapter, and so does the card of a Wikipedia page
about an event or a place that Wikipedia serves no summary of, read from the page's item:

- It takes the item's label and description: English when it has one, else the item's
  language-independent (`mul`) value, else any language (CC0, no credit).
- It shows the item's image (`P18`) only when the file's own page on Commons gives a licence on
  the list (as for [MusicBrainz enrichment](#musicbrainz-enrichment)) and, under every licence
  but CC0 and the public domain, its author. The image is Commons' thumbnail, asked
  `limits.music_image_width` (640) pixels wide, with its [credit](#credits), never a link to the
  file itself.
- Commons is asked once per reading, within two seconds and never past the card's own read. A
  file Commons refuses or does not hold gives no image. An answer that fails for a while (a
  timeout, a 429, a 5xx) keeps the credited image the card had, and the card is read again after
  at least `limits.music_enrichment_retry_secs`, as an enriched MusicBrainz card is.

## Lists

A page whose structured data holds several entries of one kind lists them: an artist's concerts,
a FAQ's questions, a category's products, a blog's posts. The kinds are events, questions,
products, articles, places, media, job postings and recipes.

**What is an entry.** An entry is an entity naming another page (its `url`, else its `@id`, is
another address), or one among several listed as a page's `mainEntity`. These are the page's
own, not entries:

- an entity naming no page (a product page's variants, a theme's and a plugin's node for one
  event);
- an entity whose `url`, `@id` or `mainEntityOfPage` is the page under any of its addresses: the
  one asked for, the one fetched, its head canonical, its `og:url` (an AMP page's article or event
  names the canonical).

This holds on the web route and the event route alike.

**The card.** Entries are parts of the page, never its subject, and are left out before the
page's JSON-LD is read, whichever scripts hold them. The card is the page's own entity (the
artist's `MusicGroup` on its concert list), or else the page (a `WebPage` named by its title),
not the first entry.

**Duplicates.** Two entries with one `url` (else one `@id`, else one name) are one. On the event
route, the same event in several scripts is read as one, whatever `@id` each script gives it:
what the first lacks (a start, a description) comes from the next.

**Event lists.** On the event route such a list yields no event, in the page's JSON-LD and in a
platform's embedded state alike. The decision rides on the card (`event.not_event`:
`lists_events`, or `creative_work` for a Wikidata item that is a work). A stored event re-read
with it is replaced, type and all. A stored event that merely lost its start for one fetch (a
consent wall, a date that failed to parse) keeps its card until the next refresh.

**Music pages.** A music object's page (a track, an album, an artist) treats its video
(`VideoObject`) as a part too: the card is the track, named by the page's title, not
"… | Music Video".

## Jobs

A page whose JSON-LD is a `JobPosting` (most job boards and career pages publish one) gets a
`JobPosting` card. So do the postings that [sources](sources/README.md) read from a job board's
or forum's API; their pages say which. The card is named by the posting's title and described by
the start of its text, as plain text.

- **Dates.** `start` is the date posted (`datePosted`; a date alone is its midnight UTC), and
  `end` the end of `validThrough` (a date alone is valid through that whole day, UTC). Search's
  `from` and `to` select by the date posted.
- **Closed postings.** A posting past its `end` is closed: `facts.job_status` says `closed` (else
  `open`), and search leaves it out unless it sets `closed=true`. Its page is read again a day
  after it closes, then no more.
- **Workplace.** The workplace (`jobLocation`) is the card's `venue`, placed on the map as an
  event's venue is ([workplace](#workplace)).
- **Remote.** A posting is remote when its `jobLocationType` is `TELECOMMUTE` or its place says
  "Remote" ("Remote - US"). It then names the regions it accepts applicants from
  (`applicantLocationRequirements`, or those its place names after "Remote"), up to 32. A place
  is read from its first 1,024 bytes; a region the cut falls in is left out. A posting can be
  remote and have a workplace.

| Fact | Meaning |
| --- | --- |
| `hiring_organization`, `hiring_organization_url` | The employer and its own site |
| `employment_type` | `FULL_TIME`, `PART_TIME`, `CONTRACTOR`, `TEMPORARY`, `INTERN`, `VOLUNTEER`, `PER_DIEM`, `OTHER` (or the posting's own word in that form); several joined with `; ` |
| `remote`, `remote_regions` | `true`, and the regions it accepts applicants from, joined with `; ` |
| `salary_min`, `salary_max`, `salary_currency`, `salary_unit` | The base salary: a range, or one amount in both; its ISO 4217 currency; per `HOUR`, `DAY`, `WEEK`, `MONTH` or `YEAR` |
| `date_posted`, `valid_through` | As the posting gives them |
| `apply_url` | Where to apply: its ApplyAction, else its own `url`, else the page when one applies there (`directApply`); left out when longer than 256 bytes |
| `job_status` | `open` or `closed`, at the time of the read |

### Workplace

The workplace is placed from the first of:

1. the page's own coordinates;
2. with a street, the address geocoded;
3. the town, when Nominatim finds exactly one settlement of that name (and country, when the
   posting names one), in no other region (state, by name or ISO code) and postal region (a
   numeric postcode's first digit) than the posting names.

A one-line place ("Springfield, IL", "Austin, TX 78701") is checked too:

- A US state's, Canadian province's or Australian state's code after the town must be the
  settlement's: "London, ON" is not London, England.
- Any code that is the settlement's own ISO 3166-2 code agrees: "Chennai, TN" is Chennai, IN-TN.
- A numeric postcode there (four digits or more, or three and two: "114 55") must be in its
  postal region.
- Other words after a town ("UK", "EU", "HQ", "District 1", a region in full) are left to
  Nominatim, which reads the whole line.

The card's `point` is then that place's, with OpenStreetMap's attribution.

## Products

A page whose JSON-LD is a `Product` (or a subtype: `ProductGroup`, `IndividualProduct`, …) is a
product card. So are the products [sources](sources/README.md) read from a shop's own product
record or a product database; their pages say which.

| Fact | Meaning |
| --- | --- |
| `gtin` | The product's barcode as GTIN-13 |
| `brand`, `sku`, `mpn` | The brand, the shop's article number, the maker's part number |
| `price`, `price_currency`, `availability`, `seller`, `price_at` | The cheapest offer's price, its ISO 4217 currency, its availability (`in_stock`, `out_of_stock`, `pre_order`, `back_order`, `discontinued`, `limited_availability`, `sold_out`, `in_store_only`, `online_only`, `reserved`, `made_to_order`), who sells it, and when that was read (RFC 3339) |
| `asin`, `isbn` | An Amazon link's ASIN, and for a book the ISBN-10 that ASIN is |

### Barcode

`facts.gtin` is the product's own GTIN as GTIN-13, with the check digit verified. It comes from
the product's `gtin13`, `gtin`, `gtin12`, `gtin14`, `gtin8` or `isbn` (a book's), or from its one
offer's.

The barcode, `sku` and `mpn` come only from the page's own product: in the page's head, or naming
the page. A page served as JSON-LD is all head, under the same rules. They never come from another
product a page lists, and a page with several products of its own (a category page's head list)
states the barcode of none.

- One product described twice (by the same `@id`, else `url`) is one product. Two `@id`s are two
  products, whatever `url` they share.
- A product naming the page only by a fragment (its `url`, else its `@id`, is `#nutella` or the
  page's URL with a fragment, in each of its descriptions) is one the page lists when the page's
  head declares the page a `CollectionPage` or `SearchResultsPage`, or names another main entity.
  A product page's `#product`, which its head names as its `mainEntity` or declares nothing
  about, is its own.
- Among products otherwise alike, one naming no page (the page's own, in its head) is the card
  over one naming another page. So is a script whose card would be the page's own over one whose
  card names another page (in a `@graph`, an array or a page's `mainEntity`, beside page metadata
  such as a `WebPage` or breadcrumbs or not).
- A `ProductGroup` names the barcode, article numbers and offers of the variant its address picks
  (its `url`, or `?variant=`), else of its only variant, never the group's. A group whose address
  picks none of several variants names no barcode.

When a service's own record states the barcode (a product database's record an
[official source](sources/README.md) reads), it is also the key `gtin:`. So the same product on
several shops is [one group](#groups), and `key=gtin:{barcode}` finds them all. A barcode that
only the page's own JSON-LD, or a record from the page's own host (a shop's product record),
states is a fact and never a key.

### Price

The price and availability are facts with the time they were read, never identity: the cheapest
offer's `price`, `price_currency`, `availability` and `seller` (all four of that one offer), and
`price_at`.

Open product databases ([sources](sources/README.md)) answer for a barcode with a derived link
and their facts, under the ODbL: show `facts.attribution` (or the card's `attribution`) with them.

### Amazon

Amazon pages are never fetched, on any of its shops (`amazon.com`, `amazon.ie`, `amazon.co.za`,
…) or short links (`amzn.to`, `a.co`, …), also where another link redirects to one (refused,
class `policy`). An Amazon product link (`/dp/{ASIN}`, `/gp/product/{ASIN}`) is a `Product` from
the link alone, with its `asin` (and `isbn`, for a book's ISBN-10). Any other Amazon link is a
`WebPage`.

## Questions

A page whose JSON-LD is a `Question` (a `QAPage`'s single main entity, as Stack Exchange's) is a
`Question` card. So are the questions [sources](sources/README.md) read from a Q&A site's API, or
a forum topic marked solved.

A `FAQPage`'s questions (its main entity, one or many) are parts of the page, never the page
itself. Its card stays the page's (`FAQPage`, a `WebPage`), named by the page's own title, as a
homepage with a FAQ block is.

A `Question` card is named by the question's title and described by the start of its text (200
characters, as a quoted post is), with its asker as `author` and when it was asked as
`published`.

| Fact | Meaning |
| --- | --- |
| `answer_count`, `score` | Its answers and votes, as its site counts them |
| `accepted_answer` | The address of the answer its asker accepted (a page's own; left out when longer than 256 bytes) |
| `closed_reason`, `duplicate_of` | Why it was closed, and the question it repeats (a link; several joined with `; `) |

A question closed as a duplicate of one other question is in that question's group (a hint, as
every group is), where its site's source says so.

## Recipes

A page whose JSON-LD is a `Recipe` is a recipe card. On a recipe page the recipe outranks the
article or video that wraps it. Only the page's own recipe does: one that names the page, or names
none. A recipe of another page that an article recommends never displaces the article, whichever
comes first and whether or not the article names its URL.

| Fact | Meaning |
| --- | --- |
| `total_minutes` | From `totalTime`, else `prepTime` plus `cookTime` |
| `recipe_yield` | As the recipe gives it (`4`, `6 servings`) |
| `ingredients` | Each `recipeIngredient` without its amount, unit (a unit word that ends the line is the ingredient: `2 cloves`), notes in parentheses, preparation after a comma (`beaten`, `chopped`, …) and alternatives (`plus …`, `or …`); lower case, joined with `; `, as many whole ones as 256 bytes hold |
| `cuisine`, `recipe_category` | As the recipe names them, joined with `; ` |
| `diet` | `gluten_free`, `vegan`, `vegetarian`, … (schema.org's `RestrictedDiet` in snake case) |
| `rating`, `rating_count` | Its aggregate rating |

Search by ingredient is `ingredient=` over that list, by whole words, not a full-text search:
`ingredient=chickpeas,lemon` lists the recipes that have both.

## Sources

Where a service's page is walled, thin or moved, Pinakes reads the service's own API for its
links instead. The read's `from` names the source, and `facts` holds the API's fields.

Most sources need no key. GitHub and CourtListener need a token (`[sources.<name>] token`, or
`PINAKES_GITHUB_TOKEN` and `PINAKES_COURTLISTENER_TOKEN`), and are off without one.
[docs/sources](sources/README.md) lists every source with the links it reads, what it fetches,
its card, its facts, its limits and licence, and its examples. Each can be switched off in
`[sources.<name>]`.

## OpenStreetMap and locations

OSM object URLs (`www.openstreetmap.org`, `openstreetmap.org`, `osm.org`, `www.osm.org`) resolve
through the OSM resolver ([places](design.md#places)). These spellings are object aliases:

- `/{type}/{id}/history[/{version}]`;
- `/edit?{type}={id}`;
- object shortlinks (`/go/<code>?{type}={id}`);
- the editing API (`api.openstreetmap.org/api/0.6/{type}/{id}[/full][.json|.xml]`);
- Nominatim `details` and `ui/details.html?osmtype=N|W|R&osmid=` (exactly one `osmtype` and
  `osmid`, no `place_id` or `postcode`).

Every spelling is an [alias](#aliases) of the canonical object URL, so one resource and one cached
`osm_place` row serve them all. OSM cards carry `attribution`; a resource read adds the category,
address and bbox.

Any other URL on an OSM host (user pages, map views, changesets, notes) is refused before any
fetch: status `error`, a link-only card, stored once as final.

How OSM answers count:

- A batch's 429 or 5xx defers its jobs, honoring a `Retry-After` of up to one hour. A 429 is the
  operator's own quota and charges nothing. A 5xx is a service's failure, as any service's busy
  answer is: its jobs dead-letter only once such failures have lasted three days.
- An oversized answer defers the jobs without a charge. Jobs claimed during that wait are put off
  to its end, without a request or a charge.
- A 401 or 403 (the operator's key refused) is read again later and never dead-letters. It is
  counted in `pinakes_source_token_refusals_total{source="osm"}`. After three days of refusals the
  link turns `unreachable`, read with the usual backoff.
- A timeout is charged from the fourth in a row (`limits.timeout_grace`).

### Points

`geo:` URIs (RFC 5870; altitude and uncertainty ignored, other CRSs are errors) and location
links resolve offline as `Place` points (`geo.enabled`): OSM marker links (`/?mlat=&mlon=`), OSM
shortlinks without an object, OsmAnd `osmand.net/map?pin=` and `osmand.net/go?lat=&lon=`, and
Organic Maps `omaps.app/<ge0 code>`. Points are not identities and never group.

Pinakes does no reverse geocoding and offers no place search. Event venues are geocoded through
Nominatim (`event.geocoding`).

### OSM dump

`GET /v1/dumps/osm_place.ndjson.gz` is the ODbL derivative database as a static file
(`application/gzip`, `Content-Disposition: attachment`). The daemon writes it every
`limits.osm_dump_every_hours` (24) from one repeatable-read snapshot. `/v1/info` gives its path
and `updated_at` in `osm_licence`.

Decompressed, its first line is the licence notice (`notice`, `licence`, `licence_url`,
`attribution_url`, `table`), then one object per cached OSM object:

| Field | Meaning |
| --- | --- |
| `osm_type`, `osm_id` | The OSM object (`node`, `way` or `relation`, positive ID) |
| `name`, `type`, `type_path`, `category` | The current name, schema.org type and its path, and OSM `key/value` category |
| `point`, `bbox` | `{lat, lon}` and `[min_lon, min_lat, max_lon, max_lat]`; `null` for missing objects |
| `address`, `tags`, `display_name` | Address parts, allowlisted OSM tags, the Nominatim display name |
| `status` | `ok`, or `missing` for cached missing records |
| `fetched_at` | When Nominatim last returned the object |
| `versions` | Every derived document served publicly, keyed by version, so earlier values behind the change journal are in the dump |

The raw fetch, the search vector and the refresh schedule are not exported. A new column is not
exported until it is added here, to the OpenAPI schema `OsmDumpPlace` and to the dump query. No
Pubky references or scores are exported.

A download reads only the file, never the database:

| Request | Answer |
| --- | --- |
| A download | 200 with `ETag`, `Last-Modified` and `Cache-Control: public, max-age=3600` |
| `If-None-Match` or `If-Modified-Since` that matches | 304, free |
| One byte range (`Range: bytes=…`, with `If-Range`) | 206; an unsatisfiable one 416 (free) |
| Over `limits.osm_dump_downloads_per_hour` (6) transfers per IP | 429 |
| Before the first file is written | 503 with `Retry-After` |

Only a transfer (200 or 206) counts against the hourly downloads; `HEAD`, 304 and 416 are free. A
new file replaces the old one atomically, so a download in progress finishes the file it began.
Coordinate-derived time zones use ODbL boundary data too; see `data/README.md`.

## Limits and errors

### Rate budget

Every public route (`/v1`, Swagger UI, the agents' guide and index, and `/mcp`) shares a per-IP
budget of `api.requests_per_minute` (120) units. The client address is the connecting address,
or, behind a reverse proxy listed in `api.trusted_proxies`, the right-most `X-Forwarded-For`
entry that is not one of them ([operations](operations.md#clients-and-budgets)).

| Request | Cost in units |
| --- | --- |
| A search with `reach` | `limits.reach_cost` (6) |
| A search with `boost` | `limits.reach_cost` (6) |
| A search with `related` | `limits.related_cost` (6) |
| A search with several of these | All of them together |
| Any other request | 1 |

The reach and boost costs are charged before any Nexus call, because each reads a set from Nexus.
Over budget, the answer is 429 with `Retry-After: 60`.

### Errors

A handler's errors are `{"error": "…"}`. So is a request the server could not read (a query, a
path, a body): it keeps its status and says what the route takes, never the input. A path no
route has (404) and a method a route does not take (405) have no body. A few answers are plain
text instead: the 405 of `GET /mcp`, and on the admin listener the 401, the 421 and
`/healthz`'s answers.

| Status | When |
| --- | --- |
| 400 | Malformed query parameters or JSON, invalid values, incompatible options |
| 404 | An unknown resource, group or link |
| 410 | A change cursor older than the retained history ([Changes](#changes)) |
| 413 | A body over 2 MiB |
| 415 | A body not sent as `application/json` |
| 422 | JSON of the wrong shape |
| 429 | Over a budget; `Retry-After` says when to try again |
| 500 | An internal error, without database details. A handler panic is answered 500 by a backstop layer (logged with its path, counted in `pinakes_api_panics_total`), never a dropped connection. |
| 501 | `wot_2` on an instance whose Nexus serves no reach set |
| 502 | Nexus failed while a search read a `reach` or `boost` set |
| 503 | The hour's new previews are taken, or the OSM dump is not written yet, with `Retry-After`; on the admin listener, the group writer is busy ([Curation](#curation)) |
| 504 | The request took longer than `limits.request_timeout_secs` (30 s) |

### Timeouts and database

The public and admin listeners share a ten-connection database pool, apart from the workers',
with a five-second statement and acquisition timeout. The daemon writes the OSM dump, not a
request: its writer's read-only transaction lifts the statement timeout to drain a large
snapshot, and the API only serves the file.

There are no routes to write resources or social state, and none for reverse geocoding or place
search.

## Agents

AI agents read Pinakes the way nexus-scout serves the social graph: through a usage guide and an
MCP endpoint. Both are on the public listener under the same per-IP budget, and neither needs a
key.

### The guide

`GET /llms.txt` (plain text) explains what Pinakes indexes, then gives a quickstart; recipes for
the questions agents ask (what is this link; places near a point with a tag; events in a city this
week; every link with one ISBN, DOI, ISRC or OSM ID; what else is the same thing); how to join
with Nexus by URI; the MCP tools; the limits; and what to do on each error. It names nexus-scout
for graph questions.

`GET /skill.md` is the same guide with skill front matter (`name: pinakes`).

The base URL in both is the host that served them: the request's `Host`, with
`X-Forwarded-Proto` when a proxy names the scheme, else `http` for a loopback host and `https`
for any other.

`GET /` returns `{service, description, start_here, links}`. `links` names the guide, the skill,
the MCP endpoint (`mcp`, when it is on), the OpenAPI document, `/v1/info` and nexus-scout's guide
(`social_graph`).

### MCP

`POST /mcp` (`api.mcp`, on by default) is a Model Context Protocol server over Streamable HTTP,
stateless, built on the official Rust SDK (`rmcp`). Messages are answered as JSON
(`application/json`); there are no sessions and no server-to-client requests.

Every tool only reads, except `card` with `resolve: true`, which stores an unknown link as a
preview and has a worker fetch it (`POST /v1/resolve`). So `card` is announced with
`readOnlyHint: false` and `openWorldHint: true`, and the others with `readOnlyHint: true` and
`openWorldHint: false`.

Each tool is a request to the public routes above, answered by their handlers under the public
listener's layers, with the caller's address:

| Tool | Request | Answer |
| --- | --- | --- |
| `search` | `GET /v1/search` with `q`, `type` (array), `exclude_type` (array), `tags` (array), `match`, `near` (`{lat, lon, radius}`), `bbox` (`[west, south, east, north]`), `from`, `to`, `key`, `remote`, `closed`, `ingredient`, `in_collection`, `host` (array), `category`, `sort`, `window`, `collapse`, `referenced`, `reach`, `boost`, `viewer`, `limit`, `cursor`, `facet` (array of `type`, `tags`, `host`, `category`) | `url`, compact cards in `items`, `cursor`, and `reach_truncated` and `facets` when present |
| `card` | `GET /v1/resources/by-uri` with `uri`; with `resolve: true` and an unknown link, `POST /v1/resolve` | `url` and one compact `card` with `category`, `address`, `facts`, `keys`, `tz`, `event_status`, `venue` and `organizer` when present, `related` and `related_total` when it has them, `aliases` (at most 10), and, when public collections hold it, `in_collections` (the first 5) and `collections_url` |
| `lookup` | `GET /v1/search?key=` with `key` and optional `limit` | As `search` |
| `group` | `GET /v1/groups/{id}` | `url`, `id`, `type`, `keys`, every member's compact card, and `best`: the most complete readable member of the group's type |
| `types` | `GET /v1/search?facet=type&limit=1` and `GET /v1/info` | `url`, the card types with their counts (`types`), and `resolves` |
| `social` | `POST /v1/resources/lookup` for card `ids`, then Nexus `POST /v1/stream/resources/by_ids` (100 URIs a call) with each card's `uri` and `refs`, and any plain `uris`; 1–100 entries together, optional `viewer` | Tags and taggers per card or link ([below](#the-social-tool)) |

A compact card keeps `id`, `uri`, `status`, `type`, `name`, `description` (at most 300
characters, then `…`), `image`, `site`, `point`, `start`, `end`, `credits`, `group`, `also_on`
(at most 8, one per service: `uri`, `site`, and `id`, `refs`, `derived`, `kind` when present),
`refs`, `collections` (when more than 0), `occurrence` and `attribution`. It adds `url`, the
card's full read (`GET /v1/resources/{id}`). A shortened description's credit is marked
`excerpted`. Every answer names the REST URL of its full result.

A failed request is the tool's error (`isError`). Its text is `{error, status, hint, url?,
retry_after_s?}`: the route's status and message, a hint on what to do, the REST URL that failed
(when a route answered), and the seconds of its `Retry-After` when it sent one (a 429, or the 503
of a new preview when the instance's hour is taken). The 429 of `social`'s own budget charge
carries no `retry_after_s`. An unknown tool is a JSON-RPC error.

#### The social tool

`social` answers per card (or link):

- `asked`: the URIs sent, the card's own first;
- `found`: those Nexus knew;
- the tags merged over them: per label, `taggers_count` (the taggers the views list, each once,
  or the most one view counts, whichever is more) and `viewer_tagged` (a view's `tag_uri` for the
  label addresses the viewer's own tag file);
- `taggers_count` over all labels, the same way.

A view counts taggers it does not list, who may be ones another view lists. Over several such
views a count is a lower bound, marked `at_least: true`.

An alias's ID (a spelling, or a card that became one after it was read) is answered under the ID
asked, with `root`, the card that answers for it. `unknown_ids` names IDs Pinakes does not know.
A plain link is sent to Nexus only when this instance holds it (a card's link or an alias of
one); `unknown_uris` names the others, which no Nexus call asks about.

All of a call's Nexus reads share one deadline, `limits.reach_timeout_secs` (5 s): past it the
tool answers 504. When Nexus does not answer the batch view, `social` is the error `502`.

#### A browser on /mcp

A `GET /mcp` that accepts `text/html` gets a short HTML page: what the endpoint is, the Claude
Code line (`claude mcp add --transport http pinakes {base}/mcp`), the JSON configuration other
clients take, the tools' names, and links to `/llms.txt` and `/skill.md`, with the base as the
guide's. Any other GET gets the 405 the protocol allows, with `Allow: POST` and one line of plain
text. Every other method is MCP's.

#### MCP budget and deadlines

- An MCP message spends one unit of the per-IP budget, and a tool call what its requests cost on
  top: a `reach` search 6, `types` two requests, `social` its lookup plus one per 100 URIs it
  asks Nexus.
- Bodies are capped at 2 MiB, as for every route. The routes keep their statement timeout, and
  `social` the Nexus client's 10-second timeout.
- A tool call has one deadline for all its requests together: two seconds inside
  `limits.request_timeout_secs`, counted from the start of the request that carries it. Past it
  the tool's error is `504`, so the message is still answered. The listener's own 504 is left for
  a message that has not arrived in full by the request's deadline.

#### No admin operation

The tools' requests go to the public routes only, built afresh with no header of the MCP request,
so the admin listener's routes and its token are out of reach. Any `Host` is accepted: the
endpoint takes no credentials and writes nothing but budgeted previews, so rmcp's DNS-rebinding
guard for local servers protects nothing here, and the public host names are the reverse proxy's
to know.

## Admin listener

`admin_listen` (default `127.0.0.1:9090`) serves the operator's routes:

| Route | Response |
| --- | --- |
| `GET /healthz` | 200 `ok` when the database answers `SELECT 1` within a second; 503 (`database unavailable`) when it does not, or (`stopping`) while the daemon is stopping (a background task ended) |
| `GET /metrics` | Prometheus text; 503 until the database gauges have been read once |
| `GET /v1/info` | The operator's state ([Admin info](#admin-info)) |
| `GET /v1/resources/{id}/document` | The stored document with its internal fields |
| `PUT /v1/groups/{id}`, `DELETE /v1/groups/{id}` | Group curation ([Curation](#curation)) |

**Authentication.** With `admin_token` (or `PINAKES__ADMIN_TOKEN`) set, every admin route but
`GET /healthz` needs `Authorization: Bearer <admin-token>`, compared in constant time. A missing
or wrong token gets 401 with `WWW-Authenticate: Bearer`. A non-loopback `admin_listen` without a
token is refused at startup.

**Host check.** The listener answers 421 to a request whose `Host` (or HTTP/2 authority) is not
`localhost`, an IP address or a name in `admin_hosts`, before the token is checked. The check
applies to `GET /healthz` too. A request with neither a `Host` nor an authority passes. The
listener sends no CORS headers.

The 401 and 421 answers, and the health route's answers, are plain text, not JSON.

### Admin info

The admin listener's `GET /v1/info` is the operator's state. It does not carry the public info's
`api_shape`, `user_agent`, `resolves`, `limits` or `attributions` ([Info](#info)):

| Field | Meaning |
| --- | --- |
| `version` | The daemon's version |
| `groups` | The `[groups]` settings |
| `manifests` | Every adapter's manifest, with whether it is on |
| `sources` | One row per source from the `source_state` table: last success, last error class, consecutive failures, change totals, held feeds, cursor resets, and the canary's state (the `canary_*` columns, such as `canary_ok` and `canary_missing`) |
| `metrics` | Every Prometheus metric family with its samples |
| `ingestion` | Per Nexus: `nexus`, `cursor`, `feed_length`, `lag`, `epoch` and `updated_at` |
| `counts` | `resources`, `aliases`, `references` and `occurrences` |
| `osm_licence` | As in the public info |
| `resolvers` | Per route its settings (`enabled`, and `concurrency`, `requests_per_second` and other settings where the route has them), and `statuses`: per route and status, `count`, `dead_letter` and `leased`. Aliases are not counted in `statuses`. |

Errors are stored only as sanitized classes. Admin info and database metrics are cached for 15
seconds. Metrics and alerts are described in [ops](../ops/README.md).

Canaries (`canary.enabled`) probe each enabled adapter every `limits.canary_interval_secs` (6
hours) under the ordinary worker and host budgets. A web canary denied by host policy is
skipped; OSM probes bypass the cache without writing it. A gauge appears only after the first
run.
