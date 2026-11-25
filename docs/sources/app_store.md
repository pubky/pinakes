# App Store

Pinakes reads apps on Apple's App Store from the iTunes Lookup API, by the app ID in the link. No
key is needed.

| | |
| --- | --- |
| Source | `app_store`, official source |
| Links | `apps.apple.com/[{country}/]app/[{name}/]id{id}`, the same paths on `itunes.apple.com` |
| Fetches | `GET https://itunes.apple.com/lookup?id={id}`; when the US storefront does not sell the app, the same with `&country={country}`, at most twice |
| Credentials | None |
| Rate | 0.33 requests per second on `itunes.apple.com` (Apple asks for about 20 calls a minute). Every source that asks this host shares the rate |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `MobileApplication`, `SoftwareApplication` |
| Keys | None of its own. The `app_stores` also-on source records `appstore:{id}` |
| Credit | None |
| Switch | `[sources.app_store] enabled`, on by default |

## Links

Every address of one app is one resource. The root is `https://apps.apple.com/app/id{id}`, and these
links are [aliases](../api.md#aliases) of it:

- `https://apps.apple.com/{country}/app/{name}/id{id}`;
- `https://apps.apple.com/{country}/app/id{id}`;
- `https://apps.apple.com/app/{name}/id{id}`;
- the same paths on `itunes.apple.com`.

The query (`?l=en`) changes nothing. An app ID is at most 20 digits without a leading zero, and
`{country}`, the storefront, is two lower-case letters. The name can be any text.

Not read here: developer pages (`/developer/…`) and pages under an app, such as its reviews.

## What it fetches

The first request is `GET https://itunes.apple.com/lookup?id={id}`, which asks the United States
storefront. When that storefront sells the app, its record is the card, whichever storefront the
link names.

When the US storefront answers no result, the source asks other storefronts with
`GET https://itunes.apple.com/lookup?id={id}&country={country}`, in this order:

1. The storefront the card was read from at its last read. This is the card's `storefront` fact,
   which is stored with the card, so it survives a restart.
2. When that storefront does not sell the app, or the card has none, the storefront that a link of
   the app named. This is the latest one seen since the daemon started. The daemon remembers it for
   up to 10,000 apps.

A storefront does not sell the app when its lookup answers no result, or answers 400 (a country code
that is no storefront). A storefront that a link named decides the card only when its lookup returns
the app. One that does not sell the app is forgotten, so tagging such a link never changes a card
read from another storefront.

An app that no asked storefront sells is `missing`. Any other answer is an error and the read is
retried: another status, a body that is not a lookup, or a record of another app. A failed lookup
keeps the remembered storefront.

## Card

The type is `SoftwareApplication` for a Mac app, else `MobileApplication`.

- `site` is "App Store", `name` the app's name and `description` its description.
- `image` is the app's icon: the 512-pixel one, else the 100-pixel one.

| Fact | Meaning |
| --- | --- |
| `developer` | The seller, else the artist the record names |
| `category` | The primary genre, for example "Social Networking" |
| `website` | The seller's website |
| `bundle_id` | Apple's bundle ID |
| `version` | The current version |
| `price` | The price as the storefront shows it, for example "Free" |
| `rating`, `rating_count` | The average rating (two decimals) and the number of ratings |
| `content_rating` | The age rating, for example "12+" |
| `released`, `updated` | When the app and its current version were released (RFC 3339) |
| `storefront` | The storefront the card was read from, for example `ch`. Absent when the card is the US record |
| `page_url` | `https://apps.apple.com/app/id{id}` |

## Keys and groups

The source records no keys. The `app_stores` also-on source (the `apps` lookup family) records the
app's own store key, `appstore:{id}`, and finds the app's other stores through Wikidata, so the
app's store links join one [group](../api.md#groups). It also compares the card's name and `website`
with other store cards, as a hint
([possibly the same app](../api.md#possibly-the-same-app)).

`bundle_id` is a fact only. It is never an Android package and joins nothing.

## Credit

The card shows no credit line.

## Turn it off

```toml
[sources.app_store]
enabled = false
```

App Store links then get ordinary web cards.

## Examples

- `https://apps.apple.com/us/app/signal-private-messenger/id874139669`
- `https://apps.apple.com/app/id874139669`
