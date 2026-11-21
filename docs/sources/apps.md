# `apps`: an app's other stores

A lookup family of [also on](../api.md#also-on) (`lookups/apps.rs`).

- `app_stores`: an app's own page on Google Play, F-Droid, the App Store, Flathub, the Snap Store
  or the Microsoft Store (never a page under it: reviews, screenshots, a developer's page, the
  store's lists) gets:
  - its own store key (below);
  - the one Wikidata item carrying that store's ID, read back and compared exactly. Of several
    items carrying it, one that another of them replaces (its "replaced by", P1366, or the
    other's "replaces", P1365) gives way to its successor: TextSecure and Signal share Signal's
    Android package, and Signal is the package's item. Several others are ambiguous and give
    nothing;
  - from that item, its other stores (and Homebrew) as links, and, only when the item carries
    one ID on the page's store, its Q-id and its store IDs (one value each) as keys: an item
    naming two packages on one store gives either page links only;
  - its `app_match` fact, from its card's name and official website (the `website` fact the
    store sources `app_store`, `google_play`, `fdroid`, `flathub` and `snap_store` give): store
    cards with the same one are [possibly the same app](../api.md#possibly-the-same-app), a hint
    for an app Wikidata does not join.

  So every store link of one app joins one [group](../api.md#groups), together with the app's
  Wikidata and Wikipedia cards.

The links it reads, and the property each store's ID has on Wikidata:

| Store | Link | Wikidata | Key |
| --- | --- | --- | --- |
| Google Play | `play.google.com/store/apps/details?id={package}` (any `hl=`, `gl=`) | P3418 | `android:{package}` |
| F-Droid | `f-droid.org/[{lang}/]packages/{package}` | P3597 | `android:{package}` |
| App Store | `apps.apple.com/[{cc}/]app/[{name}/]id{N}` | P3861 | `appstore:{N}` |
| Flathub | `flathub.org/apps/[details/]{id}` | P4655 | `flathub:{id}` |
| Snap Store | `snapcraft.io/{name}` | P4435 | `snap:{name}` |
| Microsoft Store | `apps.microsoft.com/detail/{id}`, `…/store/detail/{name}/{id}`, `microsoft.com/[{locale}/]p/{name}/{id}`, `microsoft.com/store/apps/{id}` | P5885 | `msstore:{ID}` |

Google Play and F-Droid name the same Android package, so they share `android:`: the two pages
of one package join without Wikidata. An item whose Google Play and F-Droid packages differ names
no `android:` key (either could be another build). The App Store's bundle ID is not the Android
package, and nothing joins on it.

A card on these hosts is named after its store (`site`) only when it is such an app's own page.

## Keys

`android:`, `appstore:`, `flathub:`, `snap:` and `msstore:` each name one app, so a
[group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds
the links that carry it (`key=android:org.thoughtcrime.securesms`).

## Operations

Each store link costs two Wikidata requests (the search and the item), cached as every Wikidata
read is.
