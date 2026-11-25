# Product decisions

Each entry states what Pinakes does and why. The behaviour is described in detail in
[How Pinakes works](design.md), the [API reference](api.md) and the
[source pages](sources/README.md); this page collects the reasons in one place.

## Scope

### Pinakes reads links; Nexus never does

Nexus records references to links and never fetches them. Pinakes fetches and reads them. The two
join by URI only. Pinakes never reads `pubky.app` or `/pub/social/` objects and never reads posts to
find references: the Nexus feed decides which references exist.

Why: Nexus stays small and fast and does one job, the social graph. Fetching the web is slow,
rate-limited and fails in many ways; keeping it in a separate service keeps those failures out of
Nexus.

### Pubky's own social data is never indexed

`pubky://` objects under `/pub/pubky.app/` and `/pub/social/` are never fetched and get no card.
Objects of other Pubky apps get a card when someone references them: the kinds an app's profile
describes, and any object of an app no profile describes, which gets the general card.

Why: posts, profiles, files and social events are Nexus's to describe. A second copy in Pinakes
would drift from Nexus and could outlive a deleted post.

### Nothing is watched; a card needs a reference

Pinakes does not crawl the web and does not watch homeservers or other apps' namespaces. A link
gets a card only when someone references it on Pubky or a client previews it with
`POST /v1/resolve`, or when such a link derives it (a calendar's dates, an event page's organiser
calendar). Search lists only links with at least one public reference.

Why: what people reference is the signal Pinakes indexes. Watching namespaces would make Pinakes
index posts nobody pointed at, and would need a watcher per app.

## Identity and matching

### Exact keys only

A key (an ISRC, a MusicBrainz ID, a Wikidata item, a DOI, an OSM object) comes only from a hub's
own record, a service's own record of its own object, or a registry that names a landing page the
page confirms. What a page claims about itself (`sameAs`, a canonical link, an identifier in its
markup) is at most a fact. Exact lookups ask services by identifier; a match by title, artist and
length gives only a maybe link (below).

Why: any page can claim any identifier. A wrong key would join two different things, and every
member of their groups would then show the other's tags, image and links.

### Metadata matches are "maybe", never merged

A match by metadata (the same title, artist and length; the same start time and a similar name)
is returned as `kind: "maybe"` in `also_on`, or in `maybe_same`. It never becomes a key or a group.
`kind: "maybe"` is a yes or no flag, not a confidence score. Clients may still show and play a maybe link,
after the exact ones.

Why: metadata matches are useful but can be wrong. Showing them as a hint helps readers without
letting one wrong guess spread through a group.

### Groups are hints, not merges

A group lists resources that are the same thing. Each member keeps its own card, references and
ranking; nothing is merged into the group or stored with it except membership. Cards only read
across it (Also on links, and for music a member's image when the card has none). A group holds at most one
value of each single-object namespace, so a second MusicBrainz recording or Wikidata item is
refused.

Why: a group can be wrong or change. Keeping members whole means a split loses nothing, and the
single-object rule keeps one bad claim from chaining two groups together.

### Matching works both ways and across catalogue noise

An exact link learned in one direction joins both sides, so a read from any member of a group
reaches the same Also on links, image and keys. Music services are also asked by the identifiers
other cards carry (a track's ISRC, an album's barcode), not only through MusicBrainz. When
MusicBrainz gives one ISRC to several recordings that agree (same artists, same title once version
tags are dropped, lengths within 3 seconds), they count as one recording; recordings that disagree
(live, remix, edit) stay apart.

Why: real catalogues have duplicates. Refusing an obvious match from exact data would leave the
same song unmatched across services.

### Points never join

`geo:` links and map links that name a coordinate become `Place` cards but never group, and
located group members farther apart than `limits.group_max_distance_m` (500 m) never join
automatically.

Why: a coordinate is not an identity. Two things at one spot are not the same thing.

### One event page joins only its own organiser's calendar

An event page joins the matching date in the organiser calendar its platform names for it on the
same origin (a Meetup date and its group's calendar). Two publishers' calendars never join on a
calendar UID; their events stay separate and may be listed as possibly the same event. This is
fixed behaviour with no setting.

Why: a UID is only unique within its publisher's calendar. One fixed, safe behaviour is better
than an operator switch between a safe and an unsafe mode.

### One fixed behaviour over operator switches

Where one behaviour is clearly best, Pinakes implements it without a setting. Retired settings are
ignored with a startup warning (for example `groups.cross_container_occurrences`).

Why: every switch doubles what must be tested and explained, and an operator should not have to
choose between a safe and an unsafe mode.

## Cards and content

### Card images come from open sources, by exact identifier

A MusicBrainz card takes its image and description from its Wikidata item (the Wikipedia summary,
the Wikidata description, a Wikimedia Commons image), and an album without a free Commons file
takes its front cover from the Cover Art Archive by its MBID. A Commons image is used only when
its licence is free (CC0, public domain, CC BY, CC BY-SA, the GNU GPL family or Commons'
`Attribution`) and, unless CC0 or public domain, it names its author. Each value carries its
credit.

Why: an image or text on a card must be one Pinakes can show with a correct credit, and an exact
identifier guarantees it belongs to the right thing.

### Cards fill standard fields

Enrichment fills the card's schema.org fields (`image`, `description`, `credits`) rather than
storing domain-specific data, and keeps each fact in its own field (a recording's length is
`duration`, never part of `description`). Cards store bounded values and links (an image URL,
never image bytes).

Why: one generic card shape serves every client, and clients need no per-domain code.

### Embeds come from Pinakes

A card and each `also_on` entry carry an `embed` object (the frame address, its kind, size and
provider) when the link has a player. Clients build no embed URLs themselves. Pinakes turns off
an autoplay that a stored frame's address would turn on.

Why: one place knows every provider's embed rules, and every client gets the same players.

### An identifier card names its web page

A card whose root is not a web link (`btc:`, `fr:`, `zenodo:` and other official sources'
identifier roots) names the web page its source read it from in `open_in`, from the card's
`page_url` fact.

Why: a client must be able to open the thing on the web, even for a card read on demand whose
references are empty.

### Bitcoin amounts are integer base units

Bitcoin amounts on cards are whole numbers of bitcoin base units, in fields such as `amount_sat`.
Bitcoin address cards publish the balance, totals and latest transactions, which are public chain
data.

Why: whole base units are exact and need no decimal formatting. Clients format them for display
(for example `₿994,797`).

## Social content

### People can take back what they wrote

Cards that quote a person's own words (posts, comments, notes, and profile bios on services such as
Bluesky) keep a 200-character excerpt, are read again at least every hour (Instagram's every day),
and are emptied once the original is confirmed gone: the card keeps only its link and status, with
no text, name, image, facts, Also on links or change history. A post whose author has opted out of
search on its service never matches a text search, and its text is not kept in the change history.

Why: when someone deletes a post, Pubky clients must not keep showing it. Pinakes quotes, it does
not archive.

### Some social links are read from the link alone

Facebook posts and videos, TikTok videos, LinkedIn posts and Twitch clips are link rules: their
card comes from the link's shape, and nothing is fetched. Instagram posts and reels are read from
the post's own page.

Why: those services' pages say nothing useful about a post to a request without a browser, so a
fetch would cost a request and add nothing.

## Sources

### Every source is on by default and can be turned off

Every official and also-on source ships enabled. An operator turns one off with `[sources.<name>] enabled = false` or
`[also_on.sources.<id>] enabled = false`. A source that needs a credential does not run without
one. Switching an also-on source off stops calls to its API and hides its stored rows at once, but
never hides the site: links to that site found by other sources, or tagged by people, still show.

Why: each operator decides which services to call, and switching a source off should not make a
service disappear from cards that know about it from elsewhere.

### Access walls are never worked around

A bot wall, captcha or sign-in page is a failed read. Pinakes may use what the same site publishes
elsewhere (its RSS feed, its sitemap), but never tries to get past the wall.

Why: a site that walls automated readers has said it does not want them.

### Nominatim is the only geocoder

OpenStreetMap objects and event venues are read through one Nominatim instance at
`osm.nominatim_url`; there is no provider switch and no fallback. A hosted Nominatim's URL may
carry its key, so the URL is treated as a secret and redacted from logs and `/v1/info`.

Why: Nominatim reads current OpenStreetMap data and supports the OSM-ID lookup and structured
search Pinakes needs.

### OpenStreetMap data is published as a dump

Everything Pinakes derives from OpenStreetMap is kept in one table, `osm_place`, and published as
`GET /v1/dumps/osm_place.ndjson.gz` with its licence notice, every `limits.osm_dump_every_hours`
(24).

Why: keeping every OSM-derived value in one table makes the published dump complete, with nothing
OSM-derived left elsewhere.

## API

### Search is one endpoint

Text, type, tags, identifiers (`key=`), map area, time window, reach and related items are all
parameters of `GET /v1/search`. Identifier search is part of it, not a separate lookup.

Why: clients compose filters freely, and one endpoint keeps the API small.

### Clients cluster map pins

The API has no clustering parameter. A client clusters the results of a page itself.

Why: clients cluster the page they show, so server-side clustering would add cost and no value.

### The resource type is Pinakes' reading

No API sets or changes a card's type. The type is what Pinakes reads from the link.

Why: a type set by one user would change the card for everyone.

## Operations

### The index is derived data

The index can be rebuilt by replaying the Nexus feed and reading every link again. Operator group
curation (`group_override`) and the change journal (`resource_change`) cannot, so they are the
tables to back up. A production database is upgraded in place, never dropped
([Database](operations.md#database)).

Why: keeping the index derived means a new instance needs only the Nexus feed, and code never has
to carry repairs for rows an older build stored.

### The Nexus URL holds no secret

A `nexus_url` with user info, a query or a fragment is refused. A Nexus behind credentials gets an
`Authorization` header from `PINAKES_NEXUS_AUTHORIZATION`, which is never logged and never sent to
another host.

Why: URLs end up in logs and error messages; headers can be kept out of them.
