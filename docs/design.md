# Pinakes: a Pubky index of external resources

> **Πίνακες** (Pinakes, PIN-a-keez) is the plural of *pinax*, a tablet or board, and the name of the catalogue Callimachus made of the Library of Alexandria: tables of what the library held, by subject, each entry saying what a work was and where to find it, pointing at scrolls the catalogue did not contain. It was one of the first attempts to index everything a library held. The singular covered whatever was recorded on such a tablet: a list, a register, a map of the known world. Anaximander's early world map was a pinax.
>
> The name brings together the two things this service does: keeping a record of things and showing where they are.

This page is the overview: what Pinakes is, what it needs from Nexus and what it decides. Choices marked **(arbitrary)** are defaults nothing constrains; change them freely.

## TL;DR

- People reference things outside Pubky: web pages, places, coordinates, event pages, calendar feeds, videos, other apps' objects. Nexus records the reference and never fetches, as the universal tags spec decided. Pinakes is the service that knows what those URLs are.
- Nexus publishes one feed of the references it accepted. Pinakes reads it, resolves each URL into one small typed document, keeps the facts a query needs (type, name, point, time, category) next to the references, and answers one search: text, type, map area, time, tags and reach, complete and ranked. Pinakes never parses the social specs; Nexus stays the only component that understands them.
- Nexus needs two small things: a reference feed and a batch resource view that takes URIs. Nothing about location or metadata enters Nexus, and each service keeps its own ids: they combine by URI.
- Rule for clients: Pinakes ranks, Nexus hydrates.
- Maps and Events are the first clients.

## 1. Problem {#problem}

Every external thing people reference is a `Resource` in Nexus: an id and a URI, nothing else. That is the right scope for a social indexer, and it gives a card and nothing more. "Cafés tagged cappuccino in this map area by people I follow" or "events next week" need an indexer that knows what the resource is, and filtering after Nexus's list caps gives silently wrong answers.

Maps and Events both need such an indexer. One shared indexer does the work once and makes cross-kind queries possible.


## 2. Shape {#shape}

| | Nexus | Pinakes |
|---|---|---|
| Owns | users, follows, posts, tags, collections, counts, reach; which references exist | what an external URL is and where it is; search over external resources |
| Reads | homeserver events, the social specs | Nexus's reference feed, resolver backends |
| Fetches the web | never | yes, with budgets |
| Identity | its own `Resource.id` | its own id; the two combine by URI |

Inside Pinakes, three layers: a **base** for every URI (one resource row, its references, the resolver document, the generic endpoints); **kind modules** compiled in and switched on by config, each a resolver, the facets it promotes to indexed columns and the filters it adds, never an endpoint; and **client needs** a filter cannot express: clustering and group collapse, served as modes of search. Anything that needs a user, a follow, a post body or the social specs goes to Nexus. Search's `related` filter (what a resource's taggers also referenced) is Pinakes's because it ranks typed, grouped resources over the references Pinakes already copies; Nexus keeps the plain social graph it reads them from.

Place search as you type, reverse geocoding and a place's outline are not Pinakes either, because Pinakes only knows the places someone on Pubky referenced. The Maps client calls Photon and Nominatim directly, both ours, behind the ordinary edge; Pinakes calls the same Nominatim for lookups and venue geocoding. There is no proxy service in between: in front of upstreams we run, a proxy adds code with no data of its own.


## 3. Identity {#identity}

URIs are the contract between the two services; ids are each one's own. Pinakes keys a resource on `normalize_uri` and `resource_id` in its own `identity` module: lowercase scheme and host, strip default port, userinfo and fragment, keep path and query, then sixteen bytes of BLAKE3. It reads the `uri` of each feed line and computes its id itself, never reads Nexus's, and promises no equality with it. A client combines the two by URI: a card's `uri`, and `details.aliases` for its other spellings, go to Nexus's batch view as `uris`. No crate is shared, so a third-party indexer needs only the URIs.

Identity does not fold spellings. Our writers already write one spelling: canonical OSM object URLs and one `geo:` format are writer conventions in the Maps and Events specs, and v1 rejects the `pubky<pk>` short form on the wire. A fold list has no principled end, and every change to it is a backfill in every indexer. The same object under several spellings is an alias in Pinakes, and the same thing under several URIs is a group ([5.3](#document)): both are Pinakes views that never touch ids.

Fragments are stripped, on purpose. The one place a fragment would be the only handle, an event inside a calendar feed, is an occurrence of the feed resource ([5.3](#document)), not a child URI.


## 4. What Pinakes needs from Nexus {#nexus}

1. **A reference feed.** `GET /v0/resources/changes?cursor=&limit=`, one line per reference to a non-post, non-user URI that Nexus accepted: `{op: put|del, resource_id, uri, kind: tag|reply|embed|collection, author, label?, object_path, at}`; Pinakes reads the `uri` and computes its own id. The tag and post handlers append it after they commit, the way they append to `Event:Events`; the route carries the feed epoch header; a migration seeds it from the existing `TAGGED` and `REPLIED` edges. Why: this is what makes Pinakes spec-free, epoch-free and fetch-free, and it is the feed a third-party indexer wants from a Nexus light instance.
2. **A batch resource view.** `POST /v0/stream/resources/by_ids` with `{uris: [1–100], viewer_id?}` (or Nexus's own `resource_ids`), answered in request order, mirroring `posts/by_ids`. Why: one call hydrates a map page or a feed, by the URIs Pinakes returns ([3](#identity)).

Today, on the branch `feat/resources-changes-feed` of the fork [`gillohner/pubky-nexus`](https://github.com/gillohner/pubky-nexus/tree/feat/resources-changes-feed) (not yet in pubky/pubky-nexus), the batch view exists and the feed carries tag lines only, seeded from `TAGGED` edges; reply, embed and collection lines are reserved. Reach and boost use the existing `following` and `friends` lists (pages of 200 ids); `wot_2` waits on a reach-set route (the author ids, at most 1,000, with an `ETag` and a `truncated` flag).

`Resource` nodes, `TAGGED` edges and the per-resource tag and tagger routes stay in Nexus, and the batch view joins them there; they are the social truth.

Not needed: webhooks, a plug-in trait, a point index, metadata on `Resource`, place endpoints, any `geo:` handling. Nexus does not know that a resource has a location.


## 5. How Pinakes works {#how}

### 5.1 Ingestion {#ingestion}

Poll the reference feed. `put` upserts the resource and one reference row keyed by `object_path`, so replays are idempotent and edits replace; a resource seen for the first time is queued for resolution. `del` deletes the row. The cursor moves after the batch commits. A resource with no references is dropped after seven days **(arbitrary)**.

First setup is not special: the feed is a projection of the graph. The migration that ships it seeds it from the existing edges in `indexed_at` order, and the same code is a `jobs` command that regenerates it. A regeneration bumps the epoch and Pinakes re-syncs from cursor zero. Pinakes inherits Nexus's acceptance (moderation, blacklist, epochs): one policy instead of two.

Without the feed, Pinakes would have to ingest in one of two other ways:

| | A. Reference feed | B. Pinakes watches homeservers on the `pubky-watcher` crate | C. Nexus's raw event log |
|---|---|---|---|
| Who parses the social specs | Nexus only | Nexus and Pinakes, both epochs | Nexus and Pinakes, both epochs |
| Nexus change | one append in two handlers, one route, one seed migration | none | none; the log exists |
| Pinakes carries | nothing spec-related | an unfinished watcher proof of concept, the spec crates, homeserver discovery, per-user streams | the spec crates, one body fetch per line |
| Acceptance policy | Nexus's, once | Pinakes' own; can diverge | Nexus's, once |
| Nexus down | Pinakes lags | Pinakes keeps ingesting | Pinakes lags |

B's one real advantage is independence from Nexus. It costs a second implementation of spec parsing and the half of `nexus-watcher` least worth rewriting.


### 5.2 Resolution {#resolution}

A resolver turns a URI into one small typed document, on demand and never inline with ingestion: a queue with per-resolver rate limits, backoff, dead-letter and a refresh past TTL. Four stages:

- **Classify by scheme**, no network: `geo:`, `webcal:`, `nostr:`, `doi:`, and `pubky://` objects of other apps, which get a card from the app's profile in Pinakes's code ([Pubky apps](pubky-apps.md)), or from conventional JSON members for an app without one.
- **Host adapters**, only where an API or a feed beats the page: OpenStreetMap through cached Nominatim lookups, oEmbed providers, Wikidata, DOI, event platforms such as Luma and Meetup, and the official APIs in [sources](sources/README.md).
- **Generic web**: one fetch, then JSON-LD, microformats, OpenGraph, oEmbed, title, in that order; ActivityPub when advertised; ICS and RSS by content type.
- **Chains**: an event's venue to a place from the page's own coordinates, else geocoding above a confidence threshold, else nothing. Never a guessed OSM URL.

Adapter derivation is an explicit, configurable exception to direct-reference-only fetching:
NIP-52 calendars may resolve their listed event members, and BTC Map may resolve the event's
own website. `event.adapters.derive_resources` defaults on. Per-parent and per-adapter limits
and parent-author budgets (or a synthetic per-adapter sponsor when no author exists) cover
this work. BTC Map sponsorships expire with the event (its end, else its start), capped at the
event horizon (`limits.event_months_forward`), so a far event keeps its link as the horizon
moves toward it while a year-9999 event never holds a slot past the horizon. Calendar membership
stays bounded across polls. Portal coordinates remain page-sourced venue facts,
with no writes to the OSM cache and no implicit OSM fetch.

Egress policy (every fetch a reference can steer; Pubky objects included, fetched from the homeserver's public HTTPS domain; the operator-configured Nominatim and Nexus are trusted): private ranges refused and redirects re-checked; a 10 s timeout, a 1 MB page cap (8 MB for MusicBrainz and Wikidata answers) and at most 3 redirects **(arbitrary)**; one fetch per URI per TTL no matter who asks; per-host limits with allow and deny lists; every upstream URL and rate limit in config, so a small development run can use the public Nominatim and Photon under slow limits while production uses its own planet instances. Images are never re-hosted; clients proxy.

Refresh follows the time to an event's start and stops after its end; a change is a difference in the typed document, confirmed twice before a cancellation, and becomes a line in Pinakes's own change journal. Every source is one adapter manifest (mode, cadence, identity, cancellation signal, expected fields, canary), and the same manifest drives the Prometheus metrics and Grafana alerts, so a platform that changes its markup or puts up a wall is an alert before it is a blank card.


### 5.3 Document {#document}

`type` is any schema.org type, validated against a pinned vocabulary and stored with its ancestor chain, so `type=Event` matches `MusicEvent` and `type=Place` matches `CafeOrCoffeeShop`. Inputs that are not schema.org (OpenGraph, oEmbed, ActivityStreams, OSM tags, ICS, NIP-52) map through one table.

- Common fields: name, description, image (the origin's URL), url, site, location with its source (`uri | page | geocoded | osm`), address (`street`, `house_number`, `postcode`, `city`, `country_code`), category (source-specific, for OSM `amenity/cafe`), display-only attributes, and a status (`ok | pending | missing | unreachable | error`).
- Events add start, end, a derived time zone (no site's JSON-LD carries one), all-day, status, attendance, venue and organizer.
- A calendar feed is one resource and a container. Its events are occurrence rows keyed by `(feed, UID, RECURRENCE-ID)`, since nothing in a feed URL names one event. An event that has its own page is a resource of its own once referenced, and its occurrence rows link to it.
- Caps: 32 attributes and 8 KB per document **(arbitrary)**.

**Aliases and groups.** One thing often lives at several URIs. Nexus sees as many resources as URIs, which is right: each is what someone referenced. Pinakes tells two cases apart. An **alias** is the same object under another spelling, resolved deterministically: tracking parameters, the spellings and shortlinks of one OSM object, ISBN-10 and DOI URL forms, and a page's own canonical when it differs only by a trailing slash or tracking parameters. An alias keeps its id and references but is never fetched; its references count for its root and reads answer with the root. A **group** joins resources that are one thing, only on evidence that cannot be spoofed: a DOI or ISBN card and the landing page its registry names, when that page cites the identifier in its head; one ICS UID within one feed; and links an operator curates. Claims a publisher makes about itself (sameAs, a canonical to another page, a shared homepage canonical) are not evidence: anyone can make them. A group is a hint, never a merge: every member keeps its own id, card, references, score and ranking, and each card's `also_on` lists the other members and the derived links sources found (the same thing on services nobody tagged), so a client can say "also on …" without a second request. Exact keys group too: from a hub (a MusicBrainz MBID, a Wikidata Q-id) or from a service's own record of its own object (TIDAL's ISRC of a track); a page's claim about itself is at most a fact; derived links are never fetched, counted or ranked. Facets and clusters count links, not groups. Search shows one row per group only on request (`collapse=group`, a list view): the member that passed the filters with the most references, counted up to the cap (`limits.collapse_refs_per_member`); at or above it, the lower ID, as its own card.

Cross-feed ICS UID matching is off by default (`groups.cross_container_occurrences = "off"`). The opt-in `"corroborated"` mode requires two points within 500 m, starts within 300 s, and pg_trgm similarity of normalized names ≥ 0.5; it joins one event on two platforms at the price of being copyable by a determined publisher.

Threat model for page markup: identity claims (canonical, alternate and oEmbed links, citation meta) come only from the publisher's document head, and JSON-LD `url`, `@id` and `sameAs` only from the page's own entity. They decide aliases (the same URL up to a trailing slash and tracking parameters) and identifier citations; nothing in a page body can join two resources.


### 5.4 Storage {#storage}

Postgres with PostGIS and full-text search **(arbitrary)**; any database with spatial and text indexes would do. Tables: `resource` with the facets as columns (type path, name, point, start, end, venue, category), `reference`, `occurrence`, `resource_score`, and `osm_place` on its own, because it is OSM-derived data under ODbL and is offered under ODbL with attribution while every Pubky table references it by OSM id. The score is `log1p(taggers) + 0.5·log1p(replies) + 0.3·log1p(embeds + collections)` with a 30-day half-life **(arbitrary)**; it ranks and is never shown.


### 5.5 API {#api}

Every `/v1` route returns one card, `{id, uri, status, type, name, description, image, site, point, start, end, group, also_on}` (events add `event` and `maybe_same`, occurrences `occurrence`; [api.md](api.md#card) has the full shape), and lists carry an opaque cursor; limit default 50, maximum 500 **(arbitrary)**. Score stays internal to ranking, pagination and cluster `top` selection.

| Route | What it does |
|---|---|
| `GET /v1/search?q&type&category&bbox\|near+radius&from&to&tags&match&author&venue&reach&boost&viewer&sort&cluster` | the one query over complete data, in stages: filter, collapse, rank, then page or cluster, so the first page is the true top. `facet=tags`, `facet=type` or both add label or type counts over the filtered set. `reach` with `viewer` restricts reference authors to the viewer's set; `boost` with `viewer` ranks what that set referenced higher without leaving anything out; `reach_truncated` says when a set was capped. `cluster=<zoom>` with `bbox` returns H3 clusters and points with an `ETag`: that is the map viewport; no separate endpoint, no vector tiles |
| `GET /v1/resources/{id}`, `GET /v1/resources/by-uri?uri` | the card plus its details (address, zone, venue, organizer, aliases), by ID or by any spelling of the link; a known link only, nothing is fetched |
| `POST /v1/resources/lookup` | ids or URIs, up to 500 **(arbitrary)**, for naming what a Nexus stream or a place search returned |
| `GET /v1/resources/{id}/occurrences?from&to`, `POST /v1/occurrences/lookup` | the events of a feed, or of several feeds in one time-ordered page (a client's calendars) |
| `POST /v1/resolve {uri}` | compose-time preview; three seconds synchronous, then `pending` **(arbitrary)**; open, protected per URI and per IP |
| `GET /v1/info`, admin routes | resolver status and ingestion lag; on the admin listener, group curation and a resource's stored document |
| `POST /v1/changes/lookup` | what changed: the changes to a client's own resources since its last visit, from Pinakes's change journal |
| `GET /metrics` (admin listener) | Prometheus series per source: fetch outcomes, changes, canaries, queue, ingestion lag |
| `GET /v1/groups/{id}`, `collapse=group` on search | a group's joining keys and every member's card; the optional list view with one row per group |

`tags` at most five, `match=all` at most three **(arbitrary)**. Nothing in Pinakes returns a user, a follow, a post body or a viewer's own tag state.

Consistency: Pinakes lags Nexus by a poll, and a client shows its own new reference until then. Ranks and filters are eventually consistent; what a card shows is hydrated live from Nexus, so a stale rank never shows a wrong count. `missing` resources keep their references.


## 6. Clients {#clients}

Pinakes ranks, Nexus hydrates. A filtered or sorted question goes to Pinakes; the client sends the returned cards' URIs to `resources/by_ids` for counts and its own tags (for an occurrence, `occurrence.feed_uri`: its own URL was never tagged). A screen that starts from Pubky data asks Nexus first and Pinakes' `lookup` for names and facets. At most three calls between the two; Photon and Nominatim are the client's own upstreams beside them.

- **Map viewport**: `search?bbox&cluster`, then `by_ids` for the points on screen.
- **Place panel**: `resources/{id}`, Nexus `by-uri` and the replies stream, `search?type=Event&venue=` for the events here.
- **Link card**: `lookup` for a page of posts, `resolve` while composing.
- **Events this weekend**: `search?type=Event&near&from&to&sort=start`.
- **Place search box**: Photon directly, then `lookup` to decorate the places Pubky knows.
- **Long press on the map**: Nominatim reverse directly, then a `geo:` pin or the place's OSM URI.


## 7. Decisions {#decisions}

| Decision | Basis |
|---|---|
| Ingest from a Nexus reference feed, not the homeservers and not the raw event log | one component understands the social specs; same pattern as the event log; a third party runs Nexus light in front of Pinakes |
| URIs are the contract with Nexus; each service keeps its own ids, no shared crate | a client already holds the URIs; nothing to keep in step between services; the specs fix the wire form and leave identity to indexers |
| No folding and no `geo:` rounding in identity; a spelling is an alias and the same thing at several URIs a group, both in Pinakes | ids stay what people referenced; a fold list has no end and each change is a backfill; an alias or a group is a view that changes without one |
| Groups only from evidence that cannot be spoofed (an identifier card and the landing page that cites it, one ICS UID within a feed, exact keys from a hub or a service's own record, curation) | publisher claims (sameAs, cross-path canonicals) cost nothing to make and merge unrelated pages; ranking or demotion rules cannot contain them |
| Reach and boost from the existing follow lists, truncation flagged; `wot_2` only from a Nexus reach-set route | no new Nexus work for `following` and `friends`; `wot_2` answers 501 until Nexus serves the route |
| schema.org types as they are, not a closed list | an existing vocabulary with a hierarchy beats an invented enum |
| Feed events as occurrence rows | a feed is a container, nothing in its URL names one event; an event with its own page is a resource of its own |
| Clustering as a mode of search, no viewport endpoint, no vector tiles | one query path; a tile per filter set per viewer is uncacheable |
| No re-hosting of images | a media subsystem Pinakes does not otherwise need |
| Place search, reverse and outlines from the client straight to our Photon and Nominatim; no proxy service | in front of upstreams we run, a proxy adds code with no data of its own; the marketplace proxy exists for a public upstream |
| Planet Nominatim and Photon self-hosted for production; public instances only for small development runs, under slow limits | self-hosting removes the public-instance policies and the leak of queries and coordinates to third parties |
| Three layers, modules compiled in | a runtime plugin system is what the spec ruled out of Nexus |
| Fetch health (`status`) apart from an event's lifecycle (`event_status`, an occurrence's `occurrence.status`) | a cancelled event is still a healthy resource; one field cannot mean both |
| One schema file until the first deployment; re-extraction an explicit command | every instance rebuilds from the Nexus feed in minutes; no code exists only to upgrade a database that can be rebuilt |
| Postgres with PostGIS, the caps and TTLs above, route and table names | **arbitrary** |
