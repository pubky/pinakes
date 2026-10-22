# Pubky apps

People tag objects that Pubky apps other than pubky.app keep in their public storage: a Loopky
deck, a Mapky route, a room. Pinakes fetches such a `pubky://` object from its owner's homeserver
and gives it a card. pubky.app's own objects (posts, profiles) are Nexus's and never fetched.

For an app Pinakes has a profile for, the card is that app's: a schema.org type, the name,
description, image and a few facts the app's format holds, and `open_in`, the clients that open
the object (`[{client, url}]`). A profile also says which of the app's objects get a card: every
other path in its namespace (the user's own state, private-by-intent data, files, tags) is never
fetched. An object of a listed kind whose content keeps it from a card (an unlisted room, a
withdrawn listing) is `missing` at once: its content says so, it cannot flap. An object that
is gone (404) is `missing` once a second fetch confirms it, as any link. Either way it keeps
nothing of a card it had, neither on its card nor in its change history (the lines stay, with
their kind, fields and time), and is checked again as missing links are. An object stored
before its path was left out becomes `missing` at once, without a request.

Profiles are Pinakes's own code, one file per app (`resolver/pubky_apps/<app>.rs`). Nothing is
read from the apps themselves or from a registry outside this repository.

## Any other app

An object of an app without a profile gets the general card: a `CreativeWork` whose name is
the first of `name`, `title`, `summary`, `topic`, `caption`, whose description is `description`
or `bio`, and whose image is `image`, `image_uri`, `imageUrl`, `cover` or `avatarUrl` (relative
to the object's folder). An object with none of those names or descriptions but a `content` or
`text` is a `SocialMediaPosting`: social content, a 200-character excerpt. It has no client link.

## The profiles

Paths are below `pubky://{owner}/pub/`. Every path not listed is never fetched.

### Loopky (`loopky/`)

| Path | Card |
| --- | --- |
| `decks/{deck}/manifest.json` | `LearningResource`: `title`, `description`, cover (`cover_image_ref`: its `url` when `https:`, its `uri`, or its `path` in the deck's folder); facts `card_count`, `front_lang`, `back_lang`, `topics` (the deck's own `tags`). No card while `incomplete` (a publish in progress) or when `author_pubky` is not the owner |

Opens in Loopky: `https://loopky.app/deck/?author={owner}&id={deck}`. Never fetched: the deck's
cards and media, `srs/`, `subscriptions/`, `settings.json`, `tags/`.

### Mapky (`mapky.app/`)

| Path | Card | Opens in Mapky |
| --- | --- | --- |
| `routes/{id}` | `Trip`: `name`, `description`, `image_uri`; the first waypoint as its point; facts `activity`, `distance_m`, `elevation_gain_m`, `duration_s` | `https://mapky.app/route/{owner}/{id}` |
| `geo_captures/{id}` | `Photograph` (a video, audio or 3D capture its own type): `caption`, its point; fact `capture_kind`. The media file is another object and not read | `https://mapky.app/capture/{owner}/{id}` |
| `sequences/{id}` | `ImageGallery`: `name`, `description` | `https://mapky.app/sequence/{owner}/{id}` |
| `incidents/{id}` | `SpecialAnnouncement`: `description`, its point; facts `incident_type`, `severity`. No card once `expires_at` has passed, and it is fetched again then | `https://mapky.app/incident/{owner}/{id}` |
| `reviews/{id}` | `Review`: `content`; facts `rating` (1 to 10) and `place` (the OpenStreetMap URL) | the place: `https://mapky.app/place/{type}/{id}` |
| `posts/{id}` | `SocialMediaPosting` (social content): `content`. A post about a place (its `parent`, or `place` in older posts) opens the place; a reply to a route, capture, sequence or incident opens that. An older post with `kind: review` is a `Review`; a post with `kind: collection` is a collection | the place or the parent |
| `collections/{id}` | `Collection` (the older collection objects): `name`, `description`, `image_uri`; fact `items` (how many places) | none |

Never fetched: `files/`, `blobs/`, `tags/`.

### Eventky (`eventky.app/`)

| Path | Card | Opens in Eventky |
| --- | --- | --- |
| `events/{id}` | `Event`: `summary`, `description` (else the text of `styled_description`), start and end from `dtstart`/`dtend` (or `duration`) in `dtstart_tzid` (a time without a zone is left out; a recurring event's, by its `rrule`, are its next or current occurrence's, so it is fetched again by that one until the series ends; when that occurrence is not known here, because the rule is not one Pinakes expands (yearly, monthly, weekly, daily or hourly, with at most `limits.event_max_period_dates` dates a period, 100,000 by default, counted as for calendar feeds in [the API reference](api.md)) or lies past 10,000 occurrences, the card has no time and is fetched again on the usual cadence), cancelled when `status` is `CANCELLED`; fact `location` (the first location's label) | `https://eventky.app/event/{owner}/{id}` |
| `calendars/{id}` | `Collection`: `name`, `description`; fact `timezone` | `https://eventky.app/calendar/{owner}/{id}` |

An Eventky event a Nexus event reference names has the same client link on its event card.
Images are Pubky file records, not images, and are not read. Never fetched: `attendees/`,
`tags/`.

### Pubky Rooms (`pubky-rooms/`)

| Path | Card |
| --- | --- |
| `rooms/{room}` | `Conversation`: `name`, `topic`. Only a listed room (`visibility: public`); an unlisted one has no card |

Opens in Pubky Rooms: `https://pubky-rooms-staging.fly.dev/r/{owner}/{room}` (the client's
address until rooms.pubky.app is live). Never fetched: `members/`, `messages/`, `reactions/`,
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

## Adding a Pubky app

1. Copy the smallest profile (`resolver/pubky_apps/rooms.rs`) to
   `resolver/pubky_apps/<app>.rs`. Set its namespaces, one `Kind` per object that gets a card
   (a path pattern with `{name}` segments, a `read` function, the client links), and the fact
   names its cards publish. Read the app's format leniently: members may be missing, `null` or
   in an older shape. Return `Read::NoCard` for an object its owner did not mean to list.
2. Add `pub mod <app>;` and one line in `REGISTRY` (`resolver/pubky_apps/mod.rs`).
3. Record a few real public objects in `tests/fixtures/pubky_apps/<app>/` (`<name>.json` as
   served and `<name>.uri`), check them for private data, and list them in that folder's
   README.
4. Test the profile in `tests/pubky_apps.rs`: each kind's card and links, the objects without a
   card, and the paths never fetched.
5. Describe it on this page. Cards already stored change on the next fetch, or at once with
   `pinakesd reextract pubky`.
