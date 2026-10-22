# Pubky apps

People tag objects that Pubky apps other than pubky.app keep in their public storage: a Loopky
deck, a Mapky route, a room. Pinakes fetches such a `pubky://` object from its owner's homeserver
and gives it a card. pubky.app's own objects (posts, profiles) are Nexus's and never fetched.

For an app Pinakes has a profile for, the card is that app's: a schema.org type, the name,
description, image and a few facts the app's format holds, and `open_in`, the clients that open
the object (`[{client, url}]`). A profile also says which of the app's objects get a card: every
other path in its namespace (the user's own state, private-by-intent data, files, tags) is never
fetched. An object of a listed kind whose content keeps it from a card (an unlisted room, a
deck still being uploaded) is kept as `missing` and checked again as missing links are.

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

### Pubky Rooms (`pubky-rooms/`)

| Path | Card |
| --- | --- |
| `rooms/{room}` | `Conversation`: `name`, `topic`. Only a listed room (`visibility: public`); an unlisted one has no card |

Opens in Pubky Rooms: `https://pubky-rooms-staging.fly.dev/r/{owner}/{room}` (the client's
address until rooms.pubky.app is live). Never fetched: `members/`, `messages/`, `reactions/`,
`bans/`, `mutes/`, `tags/`, `profile.json`.

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
