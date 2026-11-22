# App Store

Apps on Apple's App Store, read from the iTunes Lookup API by the app's ID. No key, no account;
Apple asks for about 20 calls a minute (one every three seconds). The descriptions and icons are
the developers' and Apple's; Apple states no data licence.

## Links

`https://apps.apple.com/app/id{id}` and every App Store address of the app are one resource:

- `https://apps.apple.com/{country}/app/{name}/id{id}`, with or without the storefront and the
  name (`/app/id{id}`, `/{country}/app/id{id}`), with any query (`?l=en`);
- the same paths on `itunes.apple.com`.

A developer's page and a page under the app (reviews) are not read.

## What it reads

`GET https://itunes.apple.com/lookup?id={id}`, the United States storefront. It answers
`text/javascript` holding JSON. When both the US storefront and another one sell the app, the
card is the US record (one resource per app). An app the US storefront does not sell answers no
result; then the read asks, with `GET https://itunes.apple.com/lookup?id={id}&country={country}`:

1. the storefront the card was read from at its last read (its `storefront` fact, kept with the
   stored card, so it holds across restarts);
2. when that one does not sell it either, or the card has none, the storefront the latest link of
   the app named since the daemon started (the country in the link).

A storefront does not sell the app when its lookup answers no result, or answers 400 (a code
that is no storefront). Any other failure of a storefront lookup (an answer that is not a lookup,
a record of another app, another status) is retried later and decides nothing.

A storefront from a link counts only once its lookup returned the app (the card then keeps it):
one that does not sell the app is forgotten once asked and never decides the card, so tagging
such a link leaves a card read from another storefront as it is. An app no asked storefront
sells is `missing`.

## Card

`MobileApplication` (an iPhone or iPad app) or `SoftwareApplication` (a Mac app), with `site`
"App Store", `name` the app's name, `description` its description and `image` its icon (512 px).

| Fact | Meaning |
| --- | --- |
| `developer` | The seller |
| `category` | The primary genre, e.g. "Social Networking" |
| `website` | The seller's website |
| `bundle_id` | Apple's bundle ID. It is never an Android package and joins nothing |
| `version` | The current version |
| `price` | The price as the storefront read shows it, e.g. "Free" |
| `rating`, `rating_count` | The average rating (two decimals) and the number of ratings |
| `content_rating` | The age rating, e.g. "12+" |
| `released`, `updated` | When the app and its current version were released (RFC 3339) |
| `storefront` | The storefront the card was read from, e.g. "ch", when the US storefront does not sell the app |
| `page_url` | `https://apps.apple.com/app/id{id}` |

The app's other stores come from the `apps` also-on family.

## Examples

- `https://apps.apple.com/us/app/signal-private-messenger/id874139669`
- `https://apps.apple.com/app/id874139669`
