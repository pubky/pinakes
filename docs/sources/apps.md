# `apps`: an app's other stores

This lookup family of [also on](../api.md#also-on) runs on an app's own page on a store. It records the
store's own ID as a key, and adds the app's other stores through the one Wikidata item that carries
that ID.

| | |
| --- | --- |
| Source | `apps`, lookup family with the one source `app_stores` |
| Runs on | An app's own page on Google Play, F-Droid, the App Store, Flathub, the Snap Store or the Microsoft Store |
| Asks | `https://www.wikidata.org/w/api.php` |
| Credentials | None |
| Rate | At most 200 requests per minute on `www.wikidata.org`; the shared default of 1 per second applies while it is lower |
| Rows live | 30 days; rows expire after twice that |
| Keys | `android:{package}`, `appstore:{N}`, `flathub:{id}`, `snap:{name}`, `msstore:{ID}`, `wikidata:{Q-id}` |
| Links | App Store, Google Play, F-Droid, Flathub, Snap Store, Microsoft Store, Homebrew. All are exact, and none carries a key |
| Facts | `app_match` |
| Switch | `[also_on.sources.app_stores] enabled`, on by default |

## What it does

### `app_stores`

The source reads these pages. A page under an app (reviews, screenshots), a developer's page and a
store's own pages are never read.

| Store | Link | Wikidata property | Key |
| --- | --- | --- | --- |
| Google Play | `play.google.com/store/apps/details?id={package}` (any `hl=`, `gl=`) | P3418 | `android:{package}` |
| F-Droid | `f-droid.org/[{lang}/]packages/{package}` | P3597 | `android:{package}` |
| App Store | `apps.apple.com/[{cc}/]app/[{name}/]id{N}` | P3861 | `appstore:{N}` |
| Flathub | `flathub.org/apps/[details/]{id}` | P4655 | `flathub:{id}` |
| Snap Store | `snapcraft.io/{name}` | P4435 | `snap:{name}` |
| Microsoft Store | `apps.microsoft.com/detail/{id}`, `apps.microsoft.com/store/detail/{name}/{id}`, `microsoft.com/{locale}/p/{name}/{id}`, `microsoft.com/store/apps/[{name}/]{id}` | P5885 | `msstore:{ID}` (upper case) |

1. The card gets its own store key, from its link alone.
2. The source searches Wikidata for items that carry the store's ID, reads them back and compares the
   ID byte for byte. Of several items, one that another of them replaces (P1366 "replaced by" or P1365
   "replaces") gives way to its successor: TextSecure and Signal share Signal's Android package, and
   Signal is the package's item. If several still carry the ID, or the search has more than five hits,
   the item is left out.
3. The card gets the item's stores and Homebrew formula as links. A property with several valid values
   adds no link.
4. When the item carries no other ID on the page's store, the page is the item's: the card also gets
   `wikidata:{Q-id}` and the item's store IDs, one value each, as keys. An item that names two packages
   on one store gives either page links only.
5. The card's name and official website give its `app_match` fact (below).

Google Play and F-Droid name the same Android package, so they share `android:`, and the two pages of
one package join without Wikidata. An item whose Google Play and F-Droid packages differ adds no
`android:` key, because either could be another build. The App Store's bundle ID is not the Android
package, and nothing joins on it.

## Keys, links and facts

`android:`, `appstore:`, `flathub:`, `snap:` and `msstore:` each name one app, so a
[group](../api.md#groups) holds at most one value of each. Every store link of one app therefore joins
one group when Wikidata bridges the stores, together with the app's Wikidata and Wikipedia cards.
[`key=`](../api.md#search) finds the links that carry a key, such as
`key=android:org.thoughtcrime.securesms`.

| Fact | Meaning |
| --- | --- |
| `app_match` | The card's website host, without `www.`, and its name in lower case, with every run of characters other than letters and digits as one space: `signal.org signal private messenger` |

The website is the card's `website` fact, which the store sources `app_store`, `google_play`,
`fdroid`, `flathub` and `snap_store` give. Store cards with the same `app_match` are
[possibly the same app](../api.md#possibly-the-same-app), a hint for an app that no Wikidata item joins.
A card without a name or a web address for its website has no `app_match`.

A card on these hosts is named after its store (`site`) only on an app's own page.

## Cost

Each store page costs one Wikidata search and one read of the items it finds, and one more read when
the search finds several. Answers are cached for `limits.discovery_cache_secs` (an hour by default)
and shared with the other sources that ask for them.

When Wikidata is busy, the run fails and is retried, and the source's stored rows stay. A Wikidata
answer over `limits.hub_body_bytes` gives nothing, and the card keeps its own key.

## Turn it off

```toml
[also_on.sources.app_stores]
enabled = false
```

After the restart, the source's stored keys, links and `app_match` facts stop showing, and groups leave
its keys out. They are kept until they expire and show again when the source is on. Cards then
have no possibly-the-same-app hint, and store pages don't join by their store IDs.
