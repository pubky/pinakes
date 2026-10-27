# Google Play

Android apps on Google Play. Google Play has no public API, so this source is a page hook: the
web route reads the app's page as usual, and the card takes the app's name, icon, developer,
developer website and category from what the page says about the app. No key, no account. The
listing is the developer's and Google's, shown under Google Play's terms.

## Links

`https://play.google.com/store/apps/details?id={package}` is the one address of an app: every
other spelling of it (a language or country, `hl=`, `gl=`, or any other parameter) is an alias.
A developer's page and the other stores on `play.google.com` are not apps.

## Pages

The page hook answers for a Google Play app page:
- its `SoftwareApplication` JSON-LD gives the name, the icon (`image`), the developer
  (`author.name`) and website (`author.url`), and the category (`applicationCategory`, as words);
- without it, the Open Graph title less its " - Apps on Google Play" suffix (in the page's
  language) and the Open Graph image.

A page that names no app keeps its own card.

## What it reads

Nothing beyond the page the web route fetched. The page is over a megabyte; the web route reads
it whole within its head budget (`egress.html_head_bytes`).

## Card

The page's card, typed `MobileApplication`, with `site` "Google Play". `details.from` is
`google_play`.

| Fact | Meaning |
| --- | --- |
| `developer` | The developer's name |
| `category` | Google Play's category, e.g. "Communication" |
| `website` | The developer's website |
| `package` | The Android package name |
| `page_url` | `https://play.google.com/store/apps/details?id={package}` |

The package is also the app's `android:` key, shared with its F-Droid page; the
`apps` also-on family joins an app's stores.

## Examples

- `https://play.google.com/store/apps/details?id=org.thoughtcrime.securesms`
