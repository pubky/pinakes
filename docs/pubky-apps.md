# Pubky apps

People tag objects that Pubky apps keep in their owners' public storage: a Loopky deck, a chat
room, a marketplace listing, a route. Pinakes fetches such a `pubky://` object from its owner's
homeserver and gives it a card. It never fetches the objects that Nexus describes: pubky.app's
own and those of the social specs.

| | |
| --- | --- |
| Route | `pubky` (`resolver/pubky.rs`) |
| Links | `pubky://{owner}/pub/{namespace}/…`: one object, named by its path |
| Fetches | The owner's homeserver from the PKARR relays, then `GET https://{homeserver}/pub/…` with a `pubky-host: {owner}` header |
| Credentials | None |
| Profiles | One per app, in `resolver/pubky_apps/` (`REGISTRY`); every other app gets the [general card](#any-other-app) |
| Never fetched | `/pub/pubky.app/…` and `/pub/social/…` (`NEVER_FETCHED`), and every path a profile does not list |
| Switch | `[pubky] enabled`, on by default |

## Links

A link names one object by its path below `/pub/`. These links get no route and no card:

- a folder: a path that ends in `/` or holds a doubled slash (`//`);
- a link with a query or a port;
- a path with a segment that decodes to a separator or a dot segment (`%2F`, `..`);
- a path outside `/pub/`.

The owner must be written as its canonical z32 key. Any other spelling of the key is refused
(`policy`).

### What is never fetched

Pinakes never fetches two namespaces (`pubky_apps::NEVER_FETCHED`), however the path spells them
(`%73ocial` is `social`):

- `pubky.app`: pubky.app's own objects, such as posts and profiles.
- `social`: the social specs' objects in any version, such as posts, events, RSVPs,
  availability, tags and ratings. Social events and availability come from Nexus.

Ingest drops a feed's post line in either namespace, so such a post creates no resource. A link
to an object there gets no route. If one is stored already, the resolver reads it as `missing`
with no card, without a request.

In an app with a profile, only the paths its profile lists are fetched. Every other path in its
namespace (the user's own state, private-by-intent data, files, tags) is treated the same way:
no route, and `missing` without a request.

## What it fetches

1. The resolver asks the PKARR relays (`[pubky] pkarr_relays`) for the owner's record and checks
   its signature locally. The record names the owner's homeserver.
2. It fetches the object from the homeserver's public HTTPS domain, with the path unchanged.
   Both requests go through the egress client: an IP address as the homeserver, a private
   address and any redirect are refused. A homeserver with only key-pinned endpoints is
   unreachable.
3. For a post, it first lists the post's folder, newest first, one entry
   (`…/{id}/?reverse=true&limit=1`), then fetches that version ([posts](#posts)).

The read has 10 seconds in all. Homeservers often serve JSON as `application/octet-stream`, so the
resolver reads the body as JSON whatever its declared type.

| Answer | Card |
| --- | --- |
| 2xx | The object's card |
| 404, 410 | `missing` |
| 401, 403 | `unreachable` |
| Other 4xx, 5xx | An error (`http_4xx`, `http_5xx`), retried as any fetch |

The card stores the body's `content_type` and `content_length`; they are not published as facts.

## Profiles

A profile is Pinakes's own code, one file per app (`resolver/pubky_apps/<app>.rs`). Nothing is
read from the apps themselves or from a registry outside this repository. A profile names:

- the app's namespaces below `/pub/`;
- each kind of object that gets a card: a path pattern, how to read it, and the clients that open
  it;
- the facts its cards publish;
- `links_off`: why its objects open in no client, while a client link is held back.

The card is the app's: a schema.org type, the name, description and image, a few facts from the
app's format, and `open_in`, the clients that open the object (`[{client, url}]`). The client URL
is filled from the owner and the path, so a card that has not been read yet already has its
`open_in`.

### When an object has no card

An object of a listed kind whose content keeps it from a card (an unlisted room, a withdrawn
listing, a post the envelope refuses) is `missing` at once. Its content says so, so it cannot
flap. An object that is gone (404) is `missing` once a second fetch confirms it, as any link.

Either way, an object in a profiled app's namespace keeps nothing of a card it had: not on its
card, and not in its change history (the lines stay, with their kind, fields and time). It is
checked again as missing links are. The same holds for a stored object whose path its profile
leaves out: it becomes `missing` without a request when it is read again, at its next refresh or
with `pinakesd reextract pubky`.

An object of an app without a profile keeps its last card under `missing`, as other links do.
Posts are the exception: a post keeps nothing, in any namespace.

### Loopky (`loopky/`)

Paths are below `pubky://{owner}/pub/loopky/`.

| Path | Card |
| --- | --- |
| `decks/{deck}/manifest.json` | `LearningResource` |

| Card field | From |
| --- | --- |
| `name` | `title` (required) |
| `description` | `description` |
| `image` | `cover_image_ref`: its `url` when `https:`, else its `uri` when `pubky:`, else its `path` in the deck's folder |

| Fact | Meaning |
| --- | --- |
| `card_count` | The number of cards in the deck |
| `front_lang` | The language of the cards' fronts |
| `back_lang` | The language of the cards' backs |
| `topics` | The deck's own `tags`, at most 10, joined by `; ` |

A deck has no card while it is `incomplete` (a publish in progress), or when its `author_pubky`
is not the owner.

Opens in Loopky: `https://loopky.app/deck/?author={owner}&id={deck}`.

Never fetched: the deck's cards and media, `srs/`, `subscriptions/`, `settings.json`, `tags/`.

### Pubky Rooms (`pubky-rooms/`)

Paths are below `pubky://{owner}/pub/pubky-rooms/`.

| Path | Card |
| --- | --- |
| `rooms/{room}` | `Conversation`: `name` (required) and `topic` as its description |

Only a listed room of the format's version 1 (`"v": 1`, `"visibility": "public"`) has a card.

The profile opens rooms in no client. Its `links_off` says why: `rooms.pubky.app` does not
resolve, and the client's address is a hosting provider's subdomain. While the pubky route is on,
`pinakesd` logs this at startup (`open_in links are off`).

Never fetched: `members/`, `messages/`, `reactions/`, `bans/`, `mutes/`, `tags/`,
`profile.json`.

### Pubky marketplace (`app.marketplace/`)

Paths are below `pubky://{owner}/pub/app.marketplace/`. Each kind is one entry in the profile.

| Path | Card | Opens in the Pubky Shop |
| --- | --- | --- |
| `v1/shop.json` | `OnlineStore` | `https://pubky.app/shop/marketplace/shop/{owner}` |
| `v1/listings/{listing}` | `Product` | `https://pubky.app/shop/marketplace/listing/{owner}/{listing}` |
| `v1/drops/{drop}` | `SaleEvent` | `https://pubky.app/shop/marketplace/drop/{owner}/{drop}` |

| Kind | Card fields |
| --- | --- |
| Shop | `name` from `name` (required), `description` from `bio`, `image` from `avatarUrl` |
| Listing | `name` from `title` (required), `description`, `image` from the first image in `media` |
| Drop | `name` from `title` (required), `description`, `image` from the first entry in `media`, `start` from `startsAt`, `end` from `endsAt` when it is after the start |

| Fact | Meaning |
| --- | --- |
| `price` | A listing's fixed price (`sale.unitPrice`) or an auction's starting price (`sale.startingPrice`), from minor units, such as `1.00 USD` |
| `condition` | A listing's `condition` |
| `category` | A listing's `categoryId` |
| `country` | A shop's or listing's `location.countryCode`, two letters, upper case |

A record must name its owner (`ownerPubky`) and its kind (`recordType`: `shop`, `listing`,
`drop`). A listing or drop must also name its own ID (`listingId`, `dropId`). A listing has a
card only while it is `active` and its auction, if it has one, has not ended. An auction's card
is fetched again when the auction ends. An adult-only listing (`adultOnly`) shows no image.

Never fetched: every other path in the namespace, such as media files, reviews, encrypted
deliverables, keys and proofs.

## Posts

Apps write their own kinds of post on the social post envelope, under their own namespace:
`/pub/{namespace}/v1/posts/{id}/{editId}.json`, each edit a new version. A post is one resource,
named by its versionless URI: `pubky://{author}/pub/{namespace}/v1/posts/{id}`. It arrives on
the resource feed as a `post` line ([posts](operations.md#posts)), or as a tagged link. The social
specs' own posts are never fetched ([what is never fetched](#what-is-never-fetched)).

To read a post, the resolver:

1. lists the post's folder newest first, one entry (`…/{id}/?reverse=true&limit=1`);
2. fetches that version file;
3. checks it with pubky-social-specs' `PostEnvelope<K>` (`=1.0.0-rc.1`), with the post's ID
   from the path. A kind is 1 to 32 characters of `a`–`z`, `0`–`9`, `_` and `-`; the kind's
   content is the profile's to read;
4. reads it by its namespace and kind (`resolver/pubky_apps/posts.rs`).

An empty folder, or a listing whose first entry is not a version file in that folder, makes the
post `missing`. A listing whose first line is not an entry of this owner is an error (`parse`),
so a homeserver that lists in another form does not turn every post `missing`.

| Namespace and kind | Card |
| --- | --- |
| `maps` `route` | `Trip` |
| `music` `session` | `BroadcastEvent` |
| `music` `listening` | `ListenAction` (now playing) |
| Any other kind in `maps` or `music` | None |
| Any other namespace, any kind | The [general card](#any-other-app), with the facts `namespace` and `kind` |

### Route

A route needs a `name` (at most 100 characters) and 2 to 100 waypoints, each with a `uri`.

- `name` and `description` (at most 2,000 characters) are the card's.
- The first waypoint, when it is a `geo:` URI, is the card's point. The box of all `geo:`
  waypoints is its `bbox`.
- Facts: `activity` (at most 20 characters), `distance_m`, `duration_s`, `elevation_gain_m`,
  `elevation_loss_m`, `waypoints` (their number) and `kind`.

### Session

A session needs the envelope's `embed` (the track) and a `started_at`. It is a `BroadcastEvent`
from `started_at` to `expires_at`, open-ended without one.

- Its name is its `name` (at most 100 characters), else `Listening session`. A session card is
  never untitled.
- Facts: `track` (the `embed` reference), `host`, `service` (from `source.service`), `author`
  (the owner's key, so the card shows whose session it is) and `kind`.

### Listening

A listening needs the envelope's `embed`, a `started_at` and an `expires_at`. It is a
`ListenAction` from `started_at` to `expires_at`.

- Its name is its `name`, else `Now playing`.
- Facts: `track`, `service`, `author` and `kind`.
- It is current from `started_at` (with a minute's grace for the writer's clock) until
  `expires_at`, for at most a day. Outside that window it has no card.

### Expiry

`started_at` and `expires_at` are instants in microseconds since the epoch. A card with an expiry
is read again when the expiry passes (`refresh_by`: the queue fetches it then, at least a minute
away, instead of at its usual time). Once the expiry has passed, the post has no card.

### Posts without a card

A `music` `listen` has no card. It counts on its track instead (an `embed` reference, once Nexus
sends one for the post). One card per play would be noise and a play-by-play history in a second
index.

A post in another namespace has no card when the namespace holds a control character or is
longer than 100 characters. A namespace such as `events` gets the general card.

## Any other app

An object of an app without a profile gets the general card, and so does a post in a namespace
without one. The Mapky and Eventky layouts (`/pub/mapky.app/…`, `/pub/eventky.app/…`) are among
these apps.

| Card field | From (the first that is set) |
| --- | --- |
| `name` | `name`, `title`, `summary`, `topic`, `caption` |
| `description` | `description`, `bio` |
| `image` | `image`, `image_uri`, `imageUrl`, `cover`, `avatarUrl`, relative to the object's folder |

An object with no name and no description but a `content` or `text` is a `SocialMediaPosting`.
It is social content: its description is a 200-character excerpt, it refreshes within the hour,
and it is purged with its history when the object is gone
([social content](design.md#social-content)). A post whose content is plain text, not a JSON
object, is read as its `content`.

A field longer than its cap (a name 200 characters, a description 2,000, an image URL 2,048), or
one that holds a NUL, is left out; the card stays. An image must be an `https:` link or a
`pubky:` link under `/pub/`, with no user name, password or port.

The general card has no client link.

## Turn it off

```toml
[pubky]
enabled = false
```

With the route off, `pubky://` links are not fetched, and `POST /v1/resolve` answers 400 for
one (`resolver is disabled`).

## Add a Pubky app

1. Copy the smallest profile (`resolver/pubky_apps/rooms.rs`) to
   `resolver/pubky_apps/<app>.rs`. Set its namespaces, one `Kind` per object that gets a card (a
   path pattern with `{name}` segments, a `read` function, the client links), and the fact names
   its cards publish.
2. Set `links_off` to `None`. While the app's client has no address Pinakes can link to, set it
   to the reason instead; the kinds then open in no client.
3. Read the app's format leniently: members may be missing, `null` or in an older shape. Return
   `Read::NoCard` for an object its owner did not mean to list.
4. Add `pub mod <app>;` and one line in `REGISTRY` (`resolver/pubky_apps/mod.rs`).
5. Record a few real public objects in `tests/fixtures/pubky_apps/<app>/` (`<name>.json` as
   served and `<name>.uri`). Check them for private data, and list them in that folder's README.
6. Test the profile in `tests/pubky_apps.rs`: each kind's card and links, the objects without a
   card, and the paths never fetched.
7. Describe it on this page.

Cards already stored change on their next fetch, or at once with `pinakesd reextract pubky`.
