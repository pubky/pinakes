# F-Droid

Pinakes reads Android apps in F-Droid's main repository by package name. F-Droid's package API says
whether the repository has the package, and the package's page gives its name, summary, icon and
links. No key is needed.

| | |
| --- | --- |
| Source | `fdroid`, official source |
| Links | `f-droid.org/[{language}/]packages/{package}` |
| Fetches | `GET https://f-droid.org/api/v1/packages/{package}`, then `GET https://f-droid.org/en/packages/{package}/` |
| Credentials | None |
| Rate | 1 request per second on `f-droid.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `MobileApplication` |
| Keys | None of its own. The `app_stores` also-on source records `android:{package}` |
| Credit | None |
| Switch | `[sources.fdroid] enabled`, on by default |

## Links

Every address of one package is one resource. The root is `https://f-droid.org/packages/{package}`,
and these links are [aliases](../api.md#aliases) of it:

- the same path with a trailing slash;
- the path under a site language, such as `/en/`, `/zh_Hant/` or `/pt_BR/`;
- either on `www.f-droid.org`.

A package has at least two parts separated by dots. Each part starts with a letter and holds
letters, digits and `_`. A package has at most 200 characters.

Not read here: category pages, the package list and any other page on the site.

## What it fetches

Two requests per read:

1. `GET https://f-droid.org/api/v1/packages/{package}`: the package's versions. A package the
   repository lacks (404) is `missing`, and the second request is not made.
2. `GET https://f-droid.org/en/packages/{package}/`: the package's English page, for its header
   (name, summary, icon) and its links. A 404 is `missing`.

An API answer for another package, a page without a package name in its header, and any other status
are errors, and the read is retried.

## Card

The type is `MobileApplication`.

- `site` is "F-Droid", `name` the app's name, `description` its summary and `image` its icon.

| Fact | Meaning |
| --- | --- |
| `website` | The app's website |
| `license` | The app's licence, as the page names it |
| `source_code` | The app's source repository |
| `version` | The name of the version the API suggests |
| `package` | The Android package |
| `page_url` | `https://f-droid.org/packages/{package}` |

## Keys and groups

The source records no keys. The `app_stores` also-on source (the `apps` lookup family) records the
key `android:{package}` from the link. The app's Google Play page carries the same key, so the two
pages of one package join one [group](../api.md#groups). The same source finds the app's other
stores through Wikidata and compares the card's name and `website` with other store cards, as a hint
([possibly the same app](../api.md#possibly-the-same-app)).

## Credit

The card shows no credit line. `license` names the app's own licence, as its page states it.

## Turn it off

```toml
[sources.fdroid]
enabled = false
```

F-Droid links then get ordinary web cards.

## Examples

- `https://f-droid.org/en/packages/de.danoeh.antennapod/`
