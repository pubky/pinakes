# Pubky apps

People tag objects that Pubky apps other than pubky.app keep in their public storage: a Loopky
deck, a room, a marketplace listing. Pinakes fetches such a `pubky://` object from its owner's
homeserver and gives it a card. pubky.app's own objects (posts, profiles) and the social specs'
objects (`/pub/social/…`, any version: posts, events, rsvps, availability, tags, ratings) are
Nexus's and never fetched: social events and availability come from Nexus.
Objects under `/pub/mapky.app/…` and `/pub/eventky.app/…` are never fetched either: routes come
as [posts](#posts), events and availability as social posts. An object of any of these namespaces
has no card, as a
left-out path of a profiled app (below): its post lines are dropped, a link to it gets no route,
and the resolver gives it no card without a request.

For an app Pinakes has a profile for, the card is that app's: a schema.org type, the name,
description, image and a few facts the app's format holds, and `open_in`, the clients that open
the object (`[{client, url}]`). A profile also says which of the app's objects get a card: every
other path in its namespace (the user's own state, private-by-intent data, files, tags) is never
fetched. An object of a listed kind whose content keeps it from a card (an unlisted room, a
withdrawn listing) is `missing` at once: its content says so, it cannot flap. An object that
is gone (404) is `missing` once a second fetch confirms it, as any link. Either way an object in
a profiled app's namespace keeps nothing of a card it had, neither on its card nor in its change
history (the lines stay, with their kind, fields and time), and is checked again as missing
links are; an object of an app without a profile keeps its last card under `missing`, as other
links do. A stored object whose path its app's profile leaves out becomes `missing` at once when
it is read again (its next refresh, or `pinakesd reextract pubky`), without a request.

Profiles are Pinakes's own code, one file per app (`resolver/pubky_apps/<app>.rs`). Nothing is
read from the apps themselves or from a registry outside this repository.

## Any other app

An object of an app without a profile, and a post of a namespace without one
([posts](#posts)), gets the general card: a `CreativeWork` whose name is
the first of `name`, `title`, `summary`, `topic`, `caption`, whose description is `description`
or `bio`, and whose image is `image`, `image_uri`, `imageUrl`, `cover` or `avatarUrl` (relative
to the object's folder). An object with none of those names or descriptions but a `content` or
`text` is a `SocialMediaPosting`: social content, a 200-character excerpt. It has no client link.
A field the database cannot store (one with a NUL) or longer than its cap (a name 200
characters, a description 2,000, an image URL 2,048) is left out; the card stays.

## The profiles

Paths are below `pubky://{owner}/pub/`. Every path not listed is never fetched.

### Loopky (`loopky/`)

| Path | Card |
| --- | --- |
| `decks/{deck}/manifest.json` | `LearningResource`: `title`, `description`, cover (`cover_image_ref`: its `url` when `https:`, its `uri`, or its `path` in the deck's folder); facts `card_count`, `front_lang`, `back_lang`, `topics` (the deck's own `tags`). No card while `incomplete` (a publish in progress) or when `author_pubky` is not the owner |

Opens in Loopky: `https://loopky.app/deck/?author={owner}&id={deck}`. Never fetched: the deck's
cards and media, `srs/`, `subscriptions/`, `settings.json`, `tags/`.

### Pubky Rooms (`pubky-rooms/`)

| Path | Card |
| --- | --- |
| `rooms/{room}` | `Conversation`: `name`, `topic`. Only a listed room (`visibility: public`); an unlisted one has no card |

No client link while rooms.pubky.app does not resolve (the client's address is a hosting
provider's subdomain): the profile's `links_off` says so, and pinakesd logs it at startup
("open_in links are off") while the pubky resolver is on. Never fetched: `members/`, `messages/`, `reactions/`,
`bans/`, `mutes/`, `tags/`, `profile.json`.

### Pubky marketplace (`app.marketplace/`)

The marketplace's own namespace, new and still changing: each kind is one entry in its profile.

| Path | Card | Opens in the Pubky Shop |
| --- | --- | --- |
| `v1/shop.json` | `OnlineStore`: `name`, `bio`, `avatarUrl`; fact `country` | `https://pubky.app/shop/marketplace/shop/{owner}` |
| `v1/listings/{listing}` | `Product`: `title`, `description`, the first image; facts `price` (the fixed price or an auction's starting one, `1.00 USD`), `condition`, `category`, `country`. Only an `active` listing whose auction (if any) has not ended, and an auction is fetched again when it ends; an adult-only listing shows no image | `https://pubky.app/shop/marketplace/listing/{owner}/{listing}` |
| `v1/drops/{drop}` | `SaleEvent`: `title`, `description`, the first image, `startsAt`, `endsAt` | `https://pubky.app/shop/marketplace/drop/{owner}/{drop}` |

A record must name its owner (`ownerPubky`) and kind (`recordType`), and a listing or drop its
own ID. Every other path in the namespace (media files, reviews, encrypted deliverables, keys,
proofs) is never fetched.

## Posts

Apps write their own kinds as posts on the social post envelope, under their own namespace:
`/pub/<namespace>/v1/posts/{id}/{editId}.json`, each edit a new version. (The social specs' own
posts, events and rsvps included, are Nexus's: never fetched.) A post is one resource, its versionless URI
(`pubky://<author>/pub/<namespace>/v1/posts/{id}`), like any other `pubky://` object: it comes on
the resource feed as a `post` line ([operations](operations.md#posts)), or is tagged, and the Pubky
resolver reads it. It lists the post's folder newest first (`…/{id}/?reverse=true&limit=1`),
fetches that version, checks it with pubky-social-specs' `PostEnvelope<K>` (`=1.0.0-rc.1`; the
kind's content is the profile's to read), and reads it by its
namespace and kind (`resolver/pubky_apps/posts.rs`):

| namespace, kind | card |
| --- | --- |
| `maps` `route` | A `Trip`: its `name`, `description`, its first waypoint as the point, the box of its `geo:` waypoints, and `activity`, `distance_m`, `duration_s`, `elevation_gain_m`, `elevation_loss_m` and the number of waypoints as facts |
| `music` `session` | A `BroadcastEvent` from `started_at` to `expires_at` (open-ended without one), with the track it `embed`s, its `host` and its `source`'s `service` as facts. Named by its optional `name` (100 characters), else "Listening session"; its author (the owner's key) is the `author` fact, so the card shows whose it is, as clients name it. Never untitled |
| `music` `listening` | Now playing: a `ListenAction` from `started_at` to `expires_at`, with the track it `embed`s and its `source`'s `service` as facts, named by its optional `name`, else "Now playing", with the `author` fact, as a session. Current from `started_at` (a minute's grace for the writer's clock), for at most a day |
| any other namespace, any kind | The general card ([any other app](#any-other-app)) of its content: a JSON object's standard fields, or a text's excerpt; with its `namespace` and `kind` as facts (a namespace with a control character, a NUL among them, or over 100 characters has no card). `events` is one (availability is a social kind) |

**Expiry.** A card with an expiry (a session's or a listening's `expires_at`) is read again when
it passes (`refresh_by`, which the queue keeps over its usual TTL, at least a minute away). Once
it has passed, the post has no card.

Any other kind in `maps` or `music` has no card, and neither has a version the envelope refuses.
A music `listen` is one: it counts on its track (an `embed` reference, once Nexus sends one for
the post), and one card per play would be noise and a play-by-play history in a second index.

The music kinds follow `pubky-music-specs` (`session`: `{started_at, expires_at?, host,
source?, name?}`; `listening`: `{started_at, expires_at, duration_ms?, source?, name?}`; the track in the
envelope's `embed`). `route` is read as the social specs' v1.1 branch defines it, since Maps'
own spec crate does not exist yet.

## Adding a Pubky app

1. Copy the smallest profile (`resolver/pubky_apps/rooms.rs`) to
   `resolver/pubky_apps/<app>.rs`. Set its namespaces, one `Kind` per object that gets a card
   (a path pattern with `{name}` segments, a `read` function, the client links), and the fact
   names its cards publish. `links_off` is `None`, or, while the app's client has no address
   Pinakes can link to, why (the kinds then open in no client). Read the app's format leniently: members may be missing, `null` or
   in an older shape. Return `Read::NoCard` for an object its owner did not mean to list.
2. Add `pub mod <app>;` and one line in `REGISTRY` (`resolver/pubky_apps/mod.rs`).
3. Record a few real public objects in `tests/fixtures/pubky_apps/<app>/` (`<name>.json` as
   served and `<name>.uri`), check them for private data, and list them in that folder's
   README.
4. Test the profile in `tests/pubky_apps.rs`: each kind's card and links, the objects without a
   card, and the paths never fetched.
5. Describe it on this page. Cards already stored change on the next fetch, or at once with
   `pinakesd reextract pubky`.
