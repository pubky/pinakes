# Pinakes: a Pubky index of external resources

Design proposal, 2026-10-14, revised after review. Author: Gil. For the Nexus team and the app teams.

> **Πίνακες** (Pinakes, PIN-a-keez) is the plural of *pinax*, a tablet or board, and the name of the catalogue Callimachus made of the Library of Alexandria: tables of what the library held, by subject, each entry saying what a work was and where to find it, pointing at scrolls the catalogue did not contain. It was one of the first attempts to index everything a library held. The singular covered whatever was recorded on such a tablet: a list, a register, a map of the known world. Anaximander's early world map was a pinax.
>
> The name brings together the two things this service does: keeping a record of things and showing where they are. The singular was the first choice; Pinax is also a web3 company that indexes blockchains, so the catalogue, not the tablet, gives the name.

This page is the overview: what Pinakes is, what it needs from Nexus and what it decides. It should be enough to judge the proposal. Every section ends with a link to the same section of the details page, which carries the arguments, the specifications and the alternatives that were considered. Choices marked **(arbitrary)** are defaults nothing constrains; change them freely.

## TL;DR

- People reference things outside Pubky: web pages, places, coordinates, event pages, calendar feeds, videos, other apps' objects. Nexus records the reference and never fetches, as the universal tags spec decided. Pinakes is the service that knows what those URLs are.
- Nexus publishes one feed of the references it accepted. Pinakes reads it, resolves each URL into one small typed document, keeps the facts a query needs (type, name, point, time, category) next to the references, and answers one search: text, type, map area, time, tags and reach, complete and ranked. Pinakes never parses the social specs; Nexus stays the only component that understands them.
- Nexus needs three small things: a reference feed, a batch resource view, and the id function as a crate. Nothing about location or metadata enters Nexus.
- Rule for clients: Pinakes ranks, Nexus hydrates.
- Nexus team, Rust, Synonym stack. I own it. Maps and Events are the first clients; their plans are the [Maps](maps.html) and [Events](events.html) pages, with the Nexus work each brings of its own.

> **What the Nexus team is asked to decide.** The reference feed against the two ingestion alternatives ([5.1](#ingestion)), the id crate ([3](#identity)), the batch resource view ([4](#nexus)).

## 1. Problem {#problem}

Every external thing people reference is a `Resource` in Nexus: an id and a URI, nothing else. That is the right scope for a social indexer, and it gives a card and nothing more. "Cafés tagged cappuccino in this map area by people I follow" or "events next week" need an indexer that knows what the resource is, and filtering after Nexus's list caps gives silently wrong answers.

Maps and Events each built a private indexer for this. One shared one does the work once and makes cross-kind queries possible.

Scale today (public graph, 2026-10-14): 921 users, 215 posts and 496 tags a week, 123 resources. Nothing here is an efficiency argument.

[Details: the problem and the scale](details.html#problem)

## 2. Shape {#shape}

| | Nexus | Pinakes |
|---|---|---|
| Owns | users, follows, posts, tags, collections, counts, reach; which references exist | what an external URL is and where it is; search over external resources |
| Reads | homeserver events, the social specs | Nexus's reference feed, resolver backends |
| Fetches the web | never | yes, with budgets |
| Identity | `Resource.id = hex(blake3(normalize_uri(uri))[0..16])` | the same id, from the same crate |

Inside Pinakes, three layers: a **base** for every URI (one resource row, its references, the resolver document, the generic endpoints); **kind modules** compiled in and switched on by config, each a resolver, the facets it promotes to indexed columns and the filters it adds, never an endpoint; and **client needs** a filter cannot express: clustering and group collapse, served as modes of search. Anything that needs a user, a follow, a post body or the social specs goes to Nexus.

Place search as you type, reverse geocoding and a place's outline are not Pinakes either, because Pinakes only knows the places someone on Pubky referenced. The Maps client calls Photon and Nominatim directly, both ours, behind the ordinary edge; Pinakes calls the same Nominatim for lookups and venue geocoding. There is no proxy service in between: in front of upstreams we run, a proxy adds code with no data of its own.

[Details: the layers, the placement rule, Photon and Nominatim, why no proxy](details.html#shape)

## 3. Identity {#identity}

Both services key on the same resource id, computed by `normalize_uri` and `resource_id` in `nexus-common` today: lowercase scheme and host, strip default port, userinfo and fragment, keep path and query, then sixteen bytes of BLAKE3. Pinakes cannot depend on that crate (it pulls Neo4j, Redis and the model layer) and a copy would drift, so the two functions move as they are into a small crate, `pubky-resource-id`, with their test vectors; `nexus-common` depends on it. No behaviour change, no backfill. Every indexer, ours or a third party's, uses the same crate. Not inside `pubky-social-specs`: the specs fix the wire form and leave the fold into an id to indexers, on purpose.

The crate does not fold spellings. Our writers already write one spelling: canonical OSM object URLs and one `geo:` format are writer conventions in the Maps and Events specs, and v1 rejects the `pubky<pk>` short form on the wire. A fold list has no principled end, and every change to it is a backfill in every indexer. The same thing under several URIs, spellings included, is a group ([5.3](#document)), a Pinakes view that never touches ids.

Fragments are stripped today, on purpose. The one place a fragment would be the only handle, an event inside a calendar feed, is an occurrence of the feed resource ([5.3](#document)), not a child URI.

[Details: the rule, why not in the specs crate, why no folding](details.html#identity)

## 4. What Pinakes needs from Nexus {#nexus}

1. **A reference feed.** `GET /v0/resources/changes?cursor=&limit=`, one line per reference to a non-post, non-user URI that Nexus accepted: `{op: put|del, resource_id, uri, kind: tag|reply|embed|collection, author, label?, object_path, at}`. The tag and post handlers append it after they commit, the way they append to `Event:Events` today; the route carries the epoch header of #1125; a migration seeds it from the existing `TAGGED` and `REPLIED` edges. Why: this is what makes Pinakes spec-free, epoch-free and fetch-free, and it is the feed a third-party indexer wants from a Nexus light instance.
2. **A batch resource view.** `POST /v0/stream/resources/by_ids` with `{resource_ids: [1–100], viewer_id?}`, a wrapper over the existing `ResourceStream::from_listed_resource_ids`, mirroring `posts/by_ids`. Why: one call hydrates a map page or a feed.
3. **The id crate.** The extraction, a pure refactor ([3](#identity)).

Later, only if a client needs it: a `reach-set` route for `wot_2`. `following` and `friends` come from the existing lists (pages of 200 ids).

Once Pinakes serves resource lists, Nexus can drop the twelve Redis resource-stream writes per tag and the `/v0/stream/resources` routes. `Resource` nodes, `TAGGED` edges, the per-resource tag and tagger routes and `by_ids` stay; they are the social truth.

Not needed: webhooks, a plug-in trait, a point index, metadata on `Resource`, place endpoints, any `geo:` handling. Nexus does not know that a resource has a location.

Maps and Events bring Nexus work of their own, on their pages: replies, embeds and collections on external resources ([Maps](maps.html#nexus)), the event kinds and one more feed line kind, `event`, carrying a start and an end ([Events](events.html#nexus)). Those are not part of the three asks above.

[Details: each ask with its reason, and what Nexus does not need to do](details.html#nexus)

## 5. How Pinakes works {#pinax}

### 5.1 Ingestion {#ingestion}

Poll the reference feed. `put` upserts the resource and one reference row keyed by `object_path`, so replays are idempotent and edits replace; a resource seen for the first time is queued for resolution. `del` deletes the row. The cursor moves after the batch commits. A resource with no references is dropped after seven days **(arbitrary)**.

First setup is not special: the feed is a projection of the graph. The migration that ships it seeds it from the existing edges in `indexed_at` order, and the same code is a `jobs` command that regenerates it. A regeneration bumps the epoch and Pinakes re-syncs from cursor zero. Pinakes inherits Nexus's acceptance (moderation, blacklist, epochs): one policy instead of two.

The feed is proposed. The two alternatives are what Pinakes would do without it, for discussion:

| | A. Reference feed (proposed) | B. Pinakes watches homeservers on the `pubky-watcher` crate | C. Nexus's raw event log |
|---|---|---|---|
| Who parses the social specs | Nexus only | Nexus and Pinakes, both epochs | Nexus and Pinakes, both epochs |
| Nexus change | one append in two handlers, one route, one seed migration | none | none; the log exists |
| Pinakes carries | nothing spec-related | an unfinished watcher proof of concept, the spec crates, homeserver discovery, per-user streams | the spec crates, one body fetch per line |
| Acceptance policy | Nexus's, once | Pinakes' own; can diverge | Nexus's, once |
| Nexus down | Pinakes lags | Pinakes keeps ingesting | Pinakes lags |

B's one real advantage is independence from Nexus. It costs a second implementation of spec parsing and the half of `nexus-watcher` least worth rewriting.

[Details: the loop line by line, seeding from the edges, the full comparison](details.html#ingestion)

### 5.2 Resolution {#resolution}

A resolver turns a URI into one small typed document, on demand and never inline with ingestion: a queue with per-resolver rate limits, backoff, dead-letter and a refresh past TTL. Four stages:

- **Classify by scheme**, no network: `geo:`, `webcal:`, `nostr:`, `doi:`, and `pubky://` objects of other apps, which get a card from conventional JSON members.
- **Host adapters**, only where an API or a feed beats the page: OpenStreetMap through cached Nominatim lookups, oEmbed providers, Wikidata, DOI, Luma and Meetup.
- **Generic web**: one fetch, then JSON-LD, microformats, OpenGraph, oEmbed, title, in that order; ActivityPub when advertised; ICS and RSS by content type.
- **Chains**: an event's venue to a place from the page's own coordinates, else geocoding above a confidence threshold, else nothing. Never a guessed OSM URL.

Adapter derivation is an explicit, configurable exception to direct-reference-only fetching:
NIP-52 calendars may resolve their listed event members, and BTC Map may resolve the event's
own website. `event.adapters.derive_resources` defaults on. Per-parent and per-adapter limits
and parent-author budgets (or a synthetic per-adapter sponsor when no author exists) cover
this work. BTC Map sponsorships expire with the event, freeing slots for new future events;
calendar membership remains bounded across polls. Portal coordinates remain page-sourced venue facts,
with no writes to the OSM cache and no implicit OSM fetch.

Egress policy: private ranges refused and redirects re-checked; a 10 s timeout, a 1 MB page cap and at most 3 redirects **(arbitrary)**; one fetch per URI per TTL no matter who asks; per-host limits with allow and deny lists; every upstream URL and rate limit in config, so development runs on the public Nominatim and Photon under slow limits and staging and production on our own planet instances. Images are never re-hosted; clients proxy.

Refresh follows the time to an event's start and stops after its end; a change is a difference in the typed document, confirmed twice before a cancellation, and becomes a line in Pinakes's own change feed. Every source is one adapter manifest (mode, cadence, identity, cancellation signal, expected fields, canary), and the same manifest drives the Prometheus metrics and Grafana alerts, so a platform that changes its markup or puts up a wall is an alert before it is a blank card ([details 5.2](details.html#changes), [5.7](details.html#operations)).

[Details: each stage, the module table, the event-source matrix, the egress policy](details.html#resolution)

### 5.3 Document {#document}

`type` is any schema.org type, validated against a pinned vocabulary and stored with its ancestor chain, so `type=Event` matches `MusicEvent` and `type=Place` matches `CafeOrCoffeeShop`. Inputs that are not schema.org (OpenGraph, oEmbed, ActivityStreams, OSM tags, ICS, NIP-52) map through one table.

- Common fields: name, description, image (the origin's URL), url, site, location with its source (`uri | page | geocoded | osm`), address (`street`, `house_number`, `postcode`, `city`, `country_code`), category (source-specific, for OSM `amenity/cafe`), display-only attributes, and a status (`ok | pending | missing | unreachable | error`).
- Events add start, end, a derived time zone (no site's JSON-LD carries one), all-day, status, attendance, venue and organizer.
- A calendar feed is one resource and a container. Its events are occurrence rows keyed by `(feed, UID, RECURRENCE-ID)`, since nothing in a feed URL names one event. An event that has its own page is a resource of its own once referenced, and its occurrence rows link to it.
- Caps: 32 attributes and 8 KB per document **(arbitrary)**.

**Groups.** One thing often lives at several URIs: a recording on Spotify, Tidal, Deezer and Qobuz, a podcast episode at its host and in every directory, a film on IMDb and Letterboxd, an event on Luma and on the organiser's site, a place under two spellings of its OSM URL. Nexus sees as many resources as URIs, which is right: each is what someone referenced. Pinakes adds a group over them, from the keys resolution finds (ISRC, podcast GUID, Wikidata and IMDb ids, ICS UID, schema.org `sameAs`, a canonical URL) and from links an operator curates, which also covers intentional merges of ids or spellings. A group is a view, never an identity: ids and references stay per URI, so no key change needs a backfill. Search collapses to one row per group on request: the unit of a result is then the group, a filter matches when any member matches, counts are distinct authors across members, a cluster counts groups, and a group's score is precomputed like a resource's. A resource read lists its group. Groups ship with the media module.

Feed-derived grouping evidence is scoped to its container resource. Cross-container
ICS UID matching is off by default (`groups.cross_container_occurrences = "off"`).
The opt-in `"corroborated"` mode requires two points within 500 m, starts within
300 s, and pg_trgm similarity of normalized names ≥ 0.5. This restores the §5.3
Luma/organiser event case at the price of being copyable by a determined publisher.
The harm bound is the automatic best-member ordering. A copier that joins with a sameAs or
canonical naming the victim's page ranks below that page, whatever its origin; only a
sameAs to an identifier-registry resource on another origin, or one the named page returns,
is exempt. That bound covers only copiers that name the victim: a corroborated copier that
names nobody is bounded by taggers alone, and a legitimate owner's one-way cross-origin sameAs
(an organiser page naming its Luma event) ranks the owner below the page it names. After that
come distinct taggers descending, references descending, admission time ascending, then
completeness. Curated best members remain pinned. Other typed IDs use publisher
boundaries from `data/multi-tenant-hosts.json`, with an operator-configurable replacement
path.

Accepted residual: on a shared registrable domain that the tenant list does not scope
(github.com, for example), every user's pages count as one publisher, so a page whose
canonical or sameAs names another user's page joins that page's group through the
same-origin rule. Claimant ranking keeps the named page as the card whatever the joiner's
taggers, and with the named page present a second claimant on another path demotes both
joiners. What remains is pollution of the membership list (and the group's tagger score) by
one joiner per key. The lever is the tenant list: adding the host with a subdomain or path-segment rule
makes each user a separate publisher, so cross-user claims no longer join.

Accepted residual: a sameAs to a URL its owner abandoned (a deleted profile or an expired
platform account) can be re-registered by someone else, whose page then names the victim back,
and the reciprocal pair joins in both modes. The lever is the publisher removing the dangling
link.

Threat model for page markup: identity claims come only from the publisher's document head
(meta, canonical/alternate/oEmbed links) and from the page's own JSON-LD entity: the chosen
entity of a head script, or of a body script that names the page or its head canonical by
`url`/`@id`. JSON-LD itself stays document-wide because plugins put it in the body. A `<script>`
injected into the body by user content is cross-site scripting on that site and outside this
threat model. Rendered user `<meta>`/`<link>` elements, which some sites allow, are not.

[Details: the two example documents, the venue chain, time zones and cancellation, groups](details.html#document)

### 5.4 Storage {#storage}

Postgres with PostGIS and full-text search **(arbitrary)**; any database with spatial and text indexes would do. Tables: `resource` with the facets as columns (type path, name, point, start, end, venue, category), `reference`, `occurrence`, `resource_score`, and `osm_place` on its own, because it is OSM-derived data under ODbL and is offered under ODbL with attribution while every Pubky table references it by OSM id. The score is `log1p(taggers) + 0.5·log1p(replies) + 0.3·log1p(embeds + collections)` with a 30-day half-life **(arbitrary)**; it ranks and is never shown.

[Details: the columns and indexes, the ODbL reasoning](details.html#storage)

### 5.5 API {#api}

`/v1` from day one. Lists return `{resource_id, uri, type, name, facets…, status}` with an opaque cursor; limit default 50, maximum 500 **(arbitrary)**. Score stays internal to ranking, pagination and cluster `top` selection.

| Route | What it does |
|---|---|
| `GET /v1/search?q&type&category&bbox\|near+radius&from&to&tags&match&author&venue&reach&viewer&sort&cluster` | the one query over complete data, in stages: filter, collapse, rank, then page or cluster, so the first page is the true top. `facet=tags` adds the label counts over the filtered set; `scheme` filters by URI scheme. `reach` with `viewer` restricts reference authors to the viewer's set, `reach_truncated` says when that set was capped. `cluster=<zoom>` with `bbox` returns H3 clusters and points with an `ETag`: that is the map viewport; no separate endpoint, no vector tiles |
| `GET /v1/resources/{id}` | one document; `fields=card\|facets\|document` on every read |
| `POST /v1/resources/lookup` | ids or URIs, up to 500 **(arbitrary)**, for naming what a Nexus stream or a place search returned |
| `GET /v1/resources/{id}/occurrences?from&to` | the events of a feed |
| `POST /v1/resolve {uri}` | compose-time preview; three seconds synchronous, then `pending` **(arbitrary)**; open, protected per URI and per IP |
| `GET /v1/info`, admin routes | resolver status and ingestion lag; re-resolve, deny a host, read budgets |
| `GET /v1/changes`, `POST /v1/changes/lookup` | what changed: Pinakes's own change feed, and the changes to a client's own resources since its last visit |
| `GET /metrics` (admin listener) | Prometheus series per source: fetch outcomes, changes, canaries, queue, ingestion lag |
| `collapse=group` on search, `GET /v1/groups/{id}` | one row per group of URIs that are one thing, with its members |

`tags` at most five, `match=all` at most three **(arbitrary)**. Nothing in Pinakes returns a user, a follow, a post body or a viewer's own tag state.

Consistency: Pinakes lags Nexus by a poll, and a client shows its own new reference until then. Ranks and filters are eventually consistent; what a card shows is hydrated live from Nexus, so a stale rank never shows a wrong count. `missing` resources keep their references.

[Details: every parameter, how search executes, the clustering response, consistency](details.html#api)

## 6. Clients {#clients}

Pinakes ranks, Nexus hydrates. A filtered or sorted question goes to Pinakes; the client sends the returned ids to `resources/by_ids` for counts and its own tags. A screen that starts from Pubky data asks Nexus first and Pinakes' `lookup` for names and facets. At most three calls between the two; Photon and Nominatim are the client's own upstreams beside them.

- **Map viewport**: `search?bbox&cluster`, then `by_ids` for the points on screen.
- **Place panel**: `resources/{id}`, Nexus `by-uri` and the replies stream, `search?type=Event&venue=` for the events here.
- **Link card**: `lookup` for a page of posts, `resolve` while composing.
- **Events this weekend**: `search?type=Event&near&from&to&sort=start`.
- **Place search box**: Photon directly, then `lookup` to decorate the places Pubky knows.
- **Long press on the map**: Nominatim reverse directly, then a `geo:` pin or the place's OSM URI.

The client plans: [Maps](maps.html) and [Events](events.html).

[Details: all ten flows, the clients of the base, the third-party kit](details.html#clients)

## 7. Decisions {#decisions}

| Decision | Basis |
|---|---|
| Ingest from a Nexus reference feed, not the homeservers and not the raw event log | one component understands the social specs; same pattern as the event log; a third party runs Nexus light in front of Pinakes |
| The id crate extracted, the rule unchanged, not inside the specs crate | every indexer, one implementation; a pure refactor; the specs fix the wire form and leave identity to indexers |
| No folding and no `geo:` rounding in identity; the same thing at several URIs is a group in Pinakes | ids stay what people referenced; a fold list has no end and each change is a backfill; a group is a view that changes without one |
| Reach from the existing follow lists, truncation flagged | no new Nexus work; `wot_2` deferred |
| schema.org types as they are, not a closed list | an existing vocabulary with a hierarchy beats an invented enum |
| Feed events as occurrence rows | a feed is a container, nothing in its URL names one event; an event with its own page is a resource of its own |
| Clustering as a mode of search, no viewport endpoint, no vector tiles | one query path; a tile per filter set per viewer is uncacheable |
| No re-hosting of images | a media subsystem Pinakes does not otherwise need |
| Place search, reverse and outlines from the client straight to our Photon and Nominatim; no proxy service | in front of upstreams we run, a proxy adds code with no data of its own; the marketplace proxy exists for a public upstream |
| Planet Nominatim and Photon self-hosted for staging and production; public instances only in development, under slow limits | self-hosting removes the public-instance policies and the leak of queries and coordinates to third parties |
| Three layers, modules compiled in | a runtime plugin system is what the spec ruled out of Nexus |
| Postgres with PostGIS, the caps and TTLs above, route and table names | **arbitrary** |

[Details: the alternatives considered and why each was rejected](details.html#decisions)

## 8. Open {#open}

1. Which schema.org vocabulary version to pin and how often to bump it.
2. Who owns the Nominatim and Photon instances: the planet import, replication, disk growth, the occasional full re-import.
3. Whether Nexus retires its resource streams once Pinakes serves lists.
4. Who delivers `event_changed` for external events: the client, through `POST /v1/changes/lookup` (proposal), or a worker that reads the change feed.

[Details: the open questions with their options](details.html#open)

## 9. Delivery order {#delivery}

1. Nexus: the reference feed, `resources/by_ids`, the id crate extracted.
2. Pinakes skeleton: feed poller, schema, `search`, `lookup`, `resolve`, queue, the `web` and `pubky` resolvers with egress policy. Link cards run on this.
3. `geo` and `osm`, clustering, the planet Nominatim and Photon imported for staging. Maps runs on this.
4. The `event` module: venue chain, feeds and occurrences, the first platform adapters. Events runs on this.
5. Media and further resolvers, groups; Nexus light plus Pinakes as the third-party kit.

[Details: the delivery order with what each step unblocks](details.html#delivery)

Review decisions: OSM object alias URLs keep separate resource identities but share the
OSM derivative row; other OSM-host pages are denied by web. OSM missing objects use the
configured 30-day negative cache after confirmation, rather than the generic short missing
TTL: a deleted object stays deleted. Web/Pubky cadence comes from `[refresh]`, not parser
`ttl_secs`. Ordinary web/Pubky validators are not persisted. Calendar validators and
`horizon_end` commit with parsed snapshots. Conditional headers are suppressed until a
snapshot exists, during missing-key/held/missing-parent confirmation, or when the stored
horizon has at most half the forward window remaining. An unconditional GET then renews
recurrence expansion even when the feed's ETag is unchanged; eligible 304s renew TTL.


Implementation note (step 4A): generic event documents, calendar occurrences,
venue geocoding and the three event search units are implemented. The companion
`design-events.md` §3 and `research-event-sources.md` §A/F are supplied separately
with the task; this file remains the overview. Lifecycle is exposed as
`event_status` to preserve the existing fetch-health `status` contract. Detailed
implemented fields and limits are maintained in `docs/api.md` and the example config.


### Media and group implementation (step 5)

Five independently switched card resolvers share the established egress and queue
policy. The pinned oEmbed registry is local data; ISBN uses Open Library, DOI uses
CSL negotiation, generic Nostr reuses signed HTTP-indexer resolution, and advertised
ActivityStreams objects use one negotiated fetch. RSS episode selection is by GUID
or matching page link, never an arbitrary item from a multi-episode feed.

`group_unit` identifies resource and occurrence members without changing upstream
IDs. `resource_group` holds directional evidence, origins, trust and admission time.
A URL claim joins only within a registrable domain or reciprocally. Cross-domain
typed identifiers require authority on both members or reciprocal links; arbitrary
self-claims cannot nominate a new best member. A landing page named by the DOI or ISBN
registry meets the publisher page's own URI or same-origin canonical, so "the paper on
doi.org and on the journal's site" groups, but registrants choose their landing URL: a
registry key naming a page on another site merges only when that page cites the identifier
back (its own `doi:`/`isbn:` key from head meta or JSON-LD). Open Library edition pages are
the registry's own site and need no citation, but only the record's own edition: other
edition ids listed in a wiki-editable record are ignored. The named page always outranks the identifier card. Only ICS UID is scoped to exact
start; occurrence joins additionally require starts within five minutes. Default
component/per-origin caps are 256/16. New excess joiners are ignored while admitted
members remain, and located members must agree within 500 m unless curated.

Grouping commits separately after resource persistence. Its own dirty queue parks
failed graphs with backoff and advances past failures. Unchanged evidence fingerprints
avoid structural rewrites. Iterative union/find and graph budgets bound work before
the revision fence. Automatic IDs have their own namespace and skip curated IDs;
curation exclusively pins its member set, permitting splits as pins or keys change.

Best-member identity and distinct-author scores are stored. Facets join memberships
by equality, and group-card aggregation happens after pagination. Resource reads
retain their own names/images; OSM-derived group cards include attribution. No OSM
facts or Nexus references move into the group tables. Singletons remain ungrouped.
