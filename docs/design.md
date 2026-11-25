# How Pinakes works

This page explains the behaviour every card shares: which links Pinakes reads, how it names them,
when it reads them again, how it joins the same thing across services, and what it never does.
Each section links to the exact rules in the [API reference](api.md), the
[operations guide](operations.md) or a [source page](sources/README.md). The reasons behind the
rules are in [decisions](decisions.md).

## Pinakes and Nexus

Nexus indexes Pubky's social graph: who tagged, replied to, embedded or collected which link. It
records those references and never fetches the links. Pinakes reads each referenced link into a
small typed card, joins cards that are the same thing, and serves search over them.

The two meet only by URI. A client asks Pinakes for cards (search, lookup, a card by link) and
asks Nexus for the social data on the same URIs (counts, tags, taggers):
**Pinakes ranks, Nexus hydrates**. Pinakes resource IDs are its own and never match Nexus IDs.

Pinakes reads these Nexus `/v1` routes:

- the resource changes feed, `GET /v1/resources/changes`, for every new or removed reference;
- the batch view `POST /v1/stream/resources/by_ids`, which the `social` tool for agents uses;
- `GET /v1/user/{viewer}/following` and `…/friends`, for `reach` and `boost` in search, and
  `…/reach-set` for `wot_2` when `nexus_reach_set` is on (off by default).

## The pipeline

```
Nexus feed ─▶ ingest ─▶ queue ─▶ resolver ─▶ card ─▶ joins ─▶ /v1 API
```

1. **Ingest.** `pinakesd` polls the Nexus feed every 5 seconds by default (`limits.poll_secs`).
   Each line names a link and a reference: a `tag`, `reply`, `embed`, `collection` item or `post`.
   Pinakes stores one reference row per reference and queues a link it has not seen.
2. **Queue.** One job per resource. A job is leased, retried with backoff and refreshed on a
   schedule ([refresh](#refresh)).
3. **Resolve.** The link's route reads it and builds one card: a schema.org type, name,
   description, image, site, place, times and facts.
4. **Join.** Background workers look the card up on hubs and services (also-on), form groups from
   exact keys, and compute "possibly the same" hints.
5. **Serve.** The public API answers search, card reads, lookups, previews, occurrences and
   changes.

A change to a stored card is written to the change journal, which clients read with
`POST /v1/changes/lookup`.

## References

A reference is one social act on a link, as Nexus reports it:

| Reference | Keyed by |
| --- | --- |
| Tag | Its author, the link and the label |
| Reply, embed, collection item | Its author, the post file and the link |
| Post | The post's version file |

Several tags on one link never fetch it more than once. Pinakes never reads `pubky.app` or `/pub/social/`
objects and never reads posts to find references: Nexus decides which references exist.

**Search lists only links with at least one public reference.** A post's own row keeps its card
alive but does not count as a reference for search. A link someone only previewed
(`POST /v1/resolve`, used while composing a post) gets a card but is not a search result. A link
that was never referenced is deleted after `limits.preview_grace_hours` (24 hours). A link whose
last reference is removed is deleted after `limits.grace_days` (7 days); a new reference in that
time keeps it.

## Routes

Every link goes to one route, chosen from the link alone without any network request
(`resolver::classify`). The first match wins:

| Route | Links |
| --- | --- |
| `official` | Links of a service whose own API Pinakes reads ([sources](sources/README.md)) |
| `isbn` | `isbn:` identifiers, read from Open Library |
| `doi` | `doi:` identifiers and `doi.org` links |
| `nostr` | `nostr:` notes and events |
| `media` | Media pages a known oEmbed provider serves |
| `pubky` | `pubky://` objects of other Pubky apps ([Pubky apps](pubky-apps.md)) |
| `osm` | OpenStreetMap objects, read through Nominatim ([places](#places)) |
| `geo` | `geo:` URIs and map links that name a point |
| `event_platform` | Event platforms (`data/event-adapters.json`) and Nostr calendar events |
| `event` | Calendar feeds (`.ics`, `webcal:`) |
| `web` | Any other `http` or `https` link |

A link with any other scheme gets no card. When the route a link needs is switched off (a card
route, an official source, calendars or an event platform), an `http` or `https` link falls back to
`web`; a link in a source's own scheme (such as `fr:`) waits until the route is on again. A link that carries a credential (a password in the user info, a
`token` or `key` parameter, a calendar's secret share link) is never previewed: `POST /v1/resolve`
answers 400. When someone references one on Pubky, it is read from its own site like any link, but
the credential appears nowhere except in the link itself, and the link is never sent to another
service ([details](api.md#resolution-and-status)).

## Identity and aliases

Pinakes normalizes a link before it names a resource: lower-case scheme and host, no default port,
no fragment and no user info; path and query stay as the URL parser reads them. A `bitcoin:`
payment link drops its query. The resource ID is the first 16
bytes of the BLAKE3 hash of the normalized link, as 32 hex characters. The ID never changes.

`http` and `https`, `www.` and a trailing slash are different links. One object under several
spellings becomes one resource through **aliases**: the alias keeps its own ID but is never
fetched, its references count for its root, and a read of the alias answers with the root's card.
Aliases come only from rules that need no guessing. Some examples:

- tracking parameters such as `utm_*` and `fbclid` are dropped;
- the spellings of one object on one service (Spotify, Apple Music, YouTube, TIDAL, Wikipedia,
  Wikidata, MusicBrainz, OpenStreetMap and every official source) fold into one root;
- an ISBN-10 or hyphenated ISBN is an alias of its ISBN-13 `isbn:` root, a DOI link of its `doi:`
  root;
- after a fetch, a page whose own canonical link differs from it only by a trailing slash or
  tracking parameters becomes an alias of that canonical.

Clients use the card's canonical `uri`, list `aliases` as other spellings, and read tags across
both. The full rule list is in [Aliases](api.md#aliases).

## Refresh

A card is read again on a schedule that depends on its route and, for dated things, on the time to
the start:

| Card | Read again |
| --- | --- |
| Official-API cards | Every day (`limits.resolver_ttl_secs`), twice as long after each read that finds the card the same, up to 90 days |
| Web and other cards without a date (not an event, a podcast show or a Pubky object) | Every 7 days, twice as long after each read that finds the card the same, up to 90 days; a change starts over |
| Starting in more than 30 days | Every 7 days |
| Starting in 7 to 30 days | Every 2 days |
| Starting in 24 hours to 7 days | Every 6 hours |
| Starting within 24 hours, or running | Every hour |
| Calendars and feeds | 1 day, 12 hours, 6 hours, 1 hour by the same tiers, or the feed's own TTL |
| Cursor feeds | Every 5 minutes |
| OpenStreetMap objects | Every 30 days (`limits.osm_ttl_secs`) |
| Social content | At least every hour; Instagram every day |

No interval is shorter than an hour, except cursor feeds and a card that names when it changes
(an auction's end), which is read again then, at least a minute later. One read at the end plus 24 hours makes
a dated resource final: it is not read again until a new reference reopens it. `geo:` points never
change and are final at once. The tier values are `limits.refresh_resource_secs` and
`limits.refresh_container_secs` ([operations](operations.md#queue-and-refresh)).

Such a link found gone for good (two fresh reads agree on a 404 or 410) is read again every 90
days until it answers again or nothing references it. A card someone opens while its refresh is
due is read again in the background.
A card built from a catalogue card is never fetched ([catalogues](#catalogues)).

A failed read keeps the last accepted card's content, though its `status` can show `error` while the
failures last. A link is `missing` only after two fresh reads agree;
for a link with an accepted card the second read comes at least an hour later. Failures back off
exponentially and dead-letter after `limits.max_failures`. When a service's API is down or
rate-limits Pinakes (the official sources, event platforms, Nominatim), the waits are not charged,
and a link dead-letters only after three days of them; a web page or calendar that answers 5xx is
an ordinary failure. Details: [Queue and refresh](operations.md#queue-and-refresh). The statuses
are explained in
[Resolution and status](api.md#resolution-and-status).

## Fetching and crawling rules

Every fetch a link can steer goes through one egress client:

- public `http` and `https` addresses only; private, loopback and cloud-metadata addresses are
  refused, and each DNS answer is checked and pinned;
- at most three redirects, each checked again; a 10-second timeout; size caps per kind of answer;
- one request at a time per host and per address, paced to `limits.host_requests_per_second`
  (1 per second by default) or the lower rate a service publishes;
- every request carries the User-Agent `pinakes/<version> (+https://pubky.app; <contact>)`, with
  the operator's contact when one is set.

Card images and embeds stay at their origin URLs: Pinakes never downloads or serves them. Nominatim
and Nexus are trusted upstreams with clients of their own. Details:
[Deployment and egress](operations.md#deployment-and-egress).

## Access walls

On the web route, a bot wall, captcha, sign-in redirect or a 401 or 403 answer is a failed read,
never a card: the link turns `unreachable` and is retried with backoff. A wall's title never
becomes a card's name. Pinakes does not work around walls.

The social-platform sources (Instagram, Facebook, LinkedIn, X and others) handle a refusal
differently: the link still gets a card of its shape alone, marked with the `blocked` fact, so it
resolves, groups and counts. A refused link is asked again a day later, then less often, up to once a
week. Two sources can make a card for a walled web page from data the site
or a registry publishes elsewhere ([sources](sources/README.md)):

- `feed`: the exact item in the same site's RSS or Atom feed or news sitemap;
- `media_records`: the exact record of a walled IMDb title page (its Wikidata item), or of a
  Pocket Casts, Castro or Overcast page that names a show's iTunes ID (its iTunes record).

A wall never counts as the object being gone, so the last accepted card stays. The markers that
detect a wall are in `data/interstitial-markers.json`
([details](operations.md#web-pages-and-access-walls)).

## Keys and groups

A **key** is an exact identifier a card carries, such as `isrc:USRC17607839`,
`mb:recording:{mbid}`, `wikidata:Q42`, `doi:10.1000/182`, `isbn:9780140328721` or
`osm:node/240109189`. A key comes only from evidence a page cannot fake:

- a hub's own record (MusicBrainz, Wikidata) that names the link;
- a service's own record of its own object (the ISRC TIDAL states for its track, the DOI Zenodo
  issued for its record);
- a registry that names a landing page, when the page cites the identifier back.

What a page says about itself (`sameAs`, a canonical link, an identifier in its markup) is at most
a fact, never a key.

A **group** joins resources that share a key: the same song on Spotify, TIDAL and MusicBrainz, a
DOI and its landing page, a calendar date and its event page. A group is a hint, never a merge:
each member keeps its own card, references and ranking, and the group stores membership only.
Guards keep one wrong claim from chaining groups together:

- a group holds at most one value of each single-object namespace (one MusicBrainz recording, one
  Wikidata item, one DOI, one OSM object, …); a join that would bring a second is refused;
- located members more than 500 m apart never join automatically (`limits.group_max_distance_m`);
- a group holds at most 256 members (`limits.group_max_members`);
- points (`geo:` links and map links) never group: coordinates are not an identity.

An operator can curate a group on the admin listener. Exact rules:
[Groups](api.md#groups).

## Also on

The also-on worker asks hubs and services about each saved card and records three things:

- **keys** (see above), which form groups;
- **facts**, such as a recording's duration or an album's barcode;
- **derived links**: the same thing on services nobody tagged, such as a song's page on Deezer
  that MusicBrainz relates to the recording.

A card's `also_on` lists its group's other members first, then derived links, one per service.
Each source has a page in [docs/sources](sources/README.md#also-on); the client view is
[Also on](api.md#also-on).

## Maybe same

Some matches rest on metadata, not an exact identifier: the same title, artist and length, or the
same start time and a similar name. Pinakes returns them, but never as a key or a group:

- a derived link matched by metadata has `kind: "maybe"` in `also_on`;
- event cards list events that are possibly the same one in `maybe_same`
  ([Possibly the same event](api.md#possibly-the-same-event));
- recording cards list possibly the same recordings
  ([Possibly the same recording](api.md#possibly-the-same-recording));
- app store cards list possibly the same app ([Possibly the same app](api.md#possibly-the-same-app)).

`kind: "maybe"` is a yes or no flag, not a confidence score; the `score` on a `maybe_same` entry
only orders the entries. A maybe link is never merged, counted or ranked, because a wrong merge would show one thing's tags, image and links on another and would
spread to every member of the group.

## Social content

Some cards quote a person's own words: a post, a comment, a note or a profile bio (Bluesky,
Mastodon, X, Reddit, Hacker News, Nostr and other sources whose page says so). These cards are
social content and follow stricter rules:

- the card keeps a short excerpt, at most 200 characters;
- the card is read again at least every hour (an Instagram card every day: Instagram refuses an
  address that asks often); while the site refuses the read, after 1 day, doubling to every 7 days
  ([refusals](operations.md#failures-and-retries));
- once the original is confirmed gone, the card keeps only its link, site and status, with the
  type `SocialMediaPosting`. Its text, name, image, facts, also-on links and change history are
  deleted, and so is what its aliases kept;
- a post whose author has opted out of search on its service (Mastodon's `indexable: false`,
  Bluesky's `!no-unauthenticated`) is marked with the `opted_out` fact: its text never matches a
  search and is not kept in the change history. Its card still previews the link.

Cards that describe a thing (a book, a song, a place) are never social content.

## Events and calendars

- An **event page** (JSON-LD `Event`, microformats `h-event`, an event platform's page) is one
  resource with a start, an end, a time zone and a venue.
- A **calendar feed** (`.ics`, `webcal:`, a platform's group calendar) is one resource of type
  `DataFeed`. Its dates are **occurrences**, read with `GET /v1/resources/{id}/occurrences`;
  recurring events are expanded.
- An event page joins the matching date in the organiser's own calendar when its platform names
  that calendar on the same origin (a Meetup date's page and its group's calendar) and the two
  start within 300 seconds. That join uses the date's calendar UID.
- Two publishers' calendars never join on a UID. Events listed on several sites stay separate and
  may be linked as possibly the same event.
- Event venues with a structured street address are geocoded through Nominatim.

Details: [Calendars and occurrences](api.md#calendars-and-occurrences) and
[Calendars and events](operations.md#calendars-and-events).

## Places

- **OpenStreetMap objects** (`openstreetmap.org/{node|way|relation}/{id}` and their other
  spellings) are read through Nominatim's lookup by OSM ID. Each object is one resource and one
  cached row in `osm_place`, read again every 30 days.
- **Points**: `geo:` URIs and map links that name a coordinate (OSM marker links, OsmAnd, Organic
  Maps) become `Place` cards offline, without any request. Points are not identities and never
  group.
- Pinakes does no reverse geocoding, no place search and no map tiles.
- The `places` also-on family ([sources](sources/README.md)) links an OSM object to its Wikidata item, and
  from there to its pages on other map and review services, only where the object and the item
  name each other.

Search filters places and events by `bbox` or by `near` and `radius` ([Search](api.md#search)).

## ODbL dump

Facts derived from OpenStreetMap live only in the `osm_place` table, and Pinakes publishes that
table under the ODbL as a file: `GET /v1/dumps/osm_place.ndjson.gz`, written every 24 hours
(`limits.osm_dump_every_hours`). Its first line is the licence notice, then one line per cached
OSM object with every version the API has served. It contains no Pubky references or scores. Each
address may download it 6 times an hour (`limits.osm_dump_downloads_per_hour`). Details:
[OpenStreetMap and locations](api.md#openstreetmap-and-locations).

## Images, embeds and credits

- `image` is the origin URL of an image the source names. Pinakes never fetches it.
- `embed` is the player or frame address for a card that has one (YouTube, Spotify, X posts, …),
  built by Pinakes from the link or the page's oEmbed answer. Pinakes never loads it; the client
  decides when to ([Embed](api.md#embed)).
- `credits` and `attribution` name what a licence requires: a Wikimedia Commons file's author and
  licence, an OpenStreetMap notice, a Zenodo record's creators ([Credits](api.md#credits)).

## Catalogues

Pinakes imports open catalogues (Wikidata, MusicBrainz, OpenAlex, Podcast Index, Tranco, software
registries) as catalogue cards ([operations](operations.md#catalogues)). A shared link that a card
holds takes its card from the catalogue instead of a fetch: by the card's own link, by a link the
catalogue lists for the item, or by an exact identifier the link names by itself (a MusicBrainz
ID in a musicbrainz.org link, a DOI, a crate's name). A name or a title never joins, for the same
reason a key never comes from a page's own claims.

Such a link is catalogue-backed. It is built at once when it is referenced, never fetched, and
built again when its card changes. Every link built from one card, and the card's related links
(an official website, which keeps its own card), share the card's group, so a client shows their
tags together. A podcast's feed is not built from its card, since a fetch reads its episodes; it
joins the card's group. When the card goes upstream or its catalogue is switched off, the link
goes back to fetching; the references stay on the link people wrote, so nothing is lost and a wrong join is
undone by the next import. OpenStreetMap cards back no link: OSM-derived values stay in
`osm_place`.

## What is never indexed

- **Pubky's own social data.** `pubky://` objects under `/pub/pubky.app/` and `/pub/social/` are
  never fetched and get no card: posts, profiles, files, events and RSVPs there are Nexus's to
  describe. Other apps' objects get a card when someone references them ([Pubky apps](pubky-apps.md)).
- **Links nobody referenced.** Pinakes does not crawl and does not watch any homeserver. A card
  exists only for a link someone referenced or previewed, or a resource such a link derives (a
  calendar's dates, an event page's organiser calendar). Reading a card fetches what that card
  needs: the link, its service's API record, documents the page names for itself (its calendar
  file, its podcast feed, its oEmbed answer), the lookups of its sources, and for a walled page the
  same site's feeds at their usual paths.
- **Image bytes and media files.** Only their URLs are stored.
- **Credentials in links.** A credential a referenced link carries is never stored anywhere but in
  the link itself, and such a link is never sent to another service.

## Storage

PostgreSQL with PostGIS and `pg_trgm`. The schema is one file, `migrations/0001_schema.sql`. The
main tables:

| Table | Holds |
| --- | --- |
| `resource` | One row per link: the card document and its indexed fields |
| `reference` | One row per social reference |
| `occurrence` | Calendar dates |
| `resource_alias` | Alias to root |
| `resource_group`, `group_override` | Group membership and operator curation |
| `resource_key`, `resource_fact`, `derived_link` | What the also-on sources found |
| `resource_change` | The change journal |
| `osm_place` | OpenStreetMap-derived facts (published as the ODbL dump) |
| `catalogue_card`, `catalogue_state` | Items of the open catalogues, one card each, and each catalogue's import state ([catalogues](operations.md#catalogues)) |
| `catalogue_join_due`, `catalogue_extras` | Links whose join with the catalogues is to be checked again, and the background extras (a Wikipedia summary) of cards built from catalogue cards ([shared links](operations.md#shared-links-a-catalogue-holds)) |

The index is derived data: replaying the Nexus feed and reading every link again rebuilds it.
Operator curation (`group_override`) and the change journal (`resource_change`) are not derived,
so those two are the tables to back up
([Database](operations.md#database)).
