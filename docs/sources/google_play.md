# Google Play

Pinakes reads Android apps on Google Play from the app's own page. Google Play has no public API, so
this source is a page hook: the web route fetches the page as usual, and the source improves that
page's card from what the page says about the app. It sends no request of its own and needs no key.

| | |
| --- | --- |
| Source | `google_play`, official source, page hook |
| Links | `play.google.com/store/apps/details?id={package}` (aliases only; the web route reads the page) |
| Fetches | Nothing beyond the page the web route fetched |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | `MobileApplication` |
| Keys | None of its own. The `app_stores` also-on source records `android:{package}` |
| Credit | None |
| Switch | `[sources.google_play] enabled`, on by default |

## Links

Every address of one app is one resource. The root is
`https://play.google.com/store/apps/details?id={package}`. The same page with other parameters (a
language `hl=`, a country `gl=`, or any other) or with a trailing slash is an
[alias](../api.md#aliases) of it.

A package has at least two parts separated by dots. Each part starts with a letter and holds
letters, digits and `_`. A package has at most 200 characters.

Not read here: developer pages, books and the other stores on `play.google.com`, and any address
whose `id` is not a package.

## What it fetches

Nothing beyond the page. An app page is over a megabyte, and the web route reads an HTML page that
long up to `limits.html_head_bytes` (4 MiB by default).

The hook runs on a page that answered 200 with HTML. It takes the app's data from:

- the page's `SoftwareApplication` JSON-LD;
- without it, the Open Graph title and image. The title counts only when it ends in " - " or " – "
  and a suffix that names Google Play, in any language ("Signal – Apps bei Google Play"). The suffix
  is dropped.

A page that names no app keeps its own web card. An unknown package's page answers 404, as the web
route reads it.

## Card

The card is the page's own card with these changes:

- The type is `MobileApplication` and `site` is "Google Play". `from` names `google_play`.
- `name` is the app's name from the JSON-LD, else from the Open Graph title.
- `image` is the JSON-LD icon, else the Open Graph image.

| Fact | Meaning |
| --- | --- |
| `developer` | The developer's name (JSON-LD `author.name`) |
| `category` | Google Play's category as words (JSON-LD `applicationCategory`), for example "Music and audio" |
| `website` | The developer's website (JSON-LD `author.url`) |
| `package` | The Android package |
| `page_url` | `https://play.google.com/store/apps/details?id={package}` |

Without the JSON-LD, only `package` and `page_url` are present.

## Keys and groups

The source records no keys. The `app_stores` also-on source (the `apps` lookup family) records the
key `android:{package}` from the link. The app's F-Droid page carries the same key, so the two pages
of one package join one [group](../api.md#groups). The same source finds the app's other stores
through Wikidata and compares the card's name and `website` with other store cards, as a hint
([possibly the same app](../api.md#possibly-the-same-app)).

## Credit

The card shows no credit line.

## Turn it off

```toml
[sources.google_play]
enabled = false
```

Google Play links then get ordinary web cards.

## Examples

- `https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms`
