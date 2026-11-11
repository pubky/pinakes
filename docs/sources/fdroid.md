# F-Droid

Android apps in F-Droid's main repository, by package name. No key, no account. F-Droid's
package API says whether the repository has the package, and the package's page gives its name,
summary, icon and links. The metadata is the apps' own and F-Droid's.

## Links

`https://f-droid.org/packages/{package}` (as Wikidata's F-Droid links spell it) and every
F-Droid address of the package are one resource: `/packages/{package}` with or without a
trailing slash, under a site language (`/en/`, `/zh_Hant/`, `/pt_BR/`), and on
`www.f-droid.org`.

## What it reads

1. `GET https://f-droid.org/api/v1/packages/{package}`: the package's versions. A package the
   repository lacks (404) is `missing`.
2. `GET https://f-droid.org/en/packages/{package}/`: the package page, read for its header
   (name, summary, icon) and its links (website, licence, source code). A page without the
   package header is an error.

## Card

`MobileApplication`, with `site` "F-Droid", `name` the app's name, `description` its summary and
`image` its icon.

| Fact | Meaning |
| --- | --- |
| `website` | The app's website |
| `license` | The app's licence, as the page names it |
| `source_code` | The app's source repository |
| `version` | The suggested version |
| `package` | The Android package name |
| `page_url` | `https://f-droid.org/packages/{package}` |

The package is also the app's `android:` key, shared with its Google Play page; the
`apps` also-on family joins them.

## Examples

- `https://f-droid.org/en/packages/de.danoeh.antennapod/`
