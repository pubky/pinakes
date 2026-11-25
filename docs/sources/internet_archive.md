# Internet Archive

Pinakes reads items in the Internet Archive (scanned books, films, audio, software, data and
collections) from the item metadata API, by identifier. No key is needed.

| | |
| --- | --- |
| Source | `internet_archive`, official source |
| Links | `ia:{identifier}`; `archive.org/details/{identifier}`, `/embed/{identifier}`, `/metadata/{identifier}` |
| Fetches | `GET https://archive.org/metadata/{identifier}/metadata` |
| Credentials | None |
| Rate | At most one request every 2 seconds on `archive.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Book`, `VideoObject`, `AudioObject`, `ImageObject`, `SoftwareApplication`, `Dataset`, `WebSite`, `Collection`, `CreativeWork` |
| Keys | None (facts only) |
| Credit | The item's licence, else its rights statement, in `attribution` |
| Switch | `[sources.internet_archive] enabled`, on by default |

## Links

Every archive.org address of one item is one resource. The root is `ia:{identifier}`, and these
links are [aliases](../api.md#aliases) of it, with or without `www.`:

- `https://archive.org/details/{identifier}`, and the reader's views of it (`…/page/…`, `…/mode/…`,
  for example `/page/n5/mode/2up`);
- `https://archive.org/embed/{identifier}`;
- `https://archive.org/metadata/{identifier}` and `…/metadata`.

An identifier is at most 100 letters, digits, `.`, `-` and `_`, and does not start with `.`.
Identifiers are case-sensitive and kept as written: `Bitcoin-Whitepaper` is not
`bitcoin-whitepaper`. An `ia:` root that is not a valid identifier is `missing` at once and never
fetched.

Not read here: a file of an item (`https://archive.org/download/{identifier}/{file}`, or its view at
`https://archive.org/details/{identifier}/{file}`), user pages (`/details/@name`), search pages and
the Wayback Machine (`web.archive.org`).

## What it fetches

One request per read: `GET https://archive.org/metadata/{identifier}/metadata`. It returns only the
item's metadata record, a few kilobytes even for an item with thousands of files.

An identifier that the Internet Archive does not know, and an item it took down ("darked"), are
`missing`. Any other error the API answers, or an answer for another item, is an error, and the read
is retried.

## Card

The type follows the item's `mediatype`:

| `mediatype` | Type |
| --- | --- |
| `texts` | `Book` |
| `movies` | `VideoObject` |
| `audio`, `etree` | `AudioObject` |
| `image` | `ImageObject` |
| `software` | `SoftwareApplication` |
| `data` | `Dataset` |
| `web` | `WebSite` |
| `collection` | `Collection` |
| Any other | `CreativeWork` |

- `site` is "Internet Archive", `name` the title, `description` the plain text of the item's
  description.
- `image` is the item's thumbnail (`https://archive.org/services/img/{identifier}`), only for an
  item under CC0, the Public Domain Mark or CC BY without further terms.
- `attribution` is the short name of a Creative Commons licence, else the licence's URL, else the
  item's rights statement.
- The item's page is the `page_url` fact. Because the root `ia:{identifier}` is not a web link, the
  API also lists that page in `open_in`.

| Fact | Meaning |
| --- | --- |
| `creators` | The creators, as the item names them |
| `date` | The item's date as written (`c1888`, `2008-10-31`, …) |
| `publishers` | The publishers |
| `languages` | Language codes, for example `eng` |
| `collections` | The collections the item is in |
| `subjects` | Subjects and keywords |
| `mediatype` | `texts`, `movies`, `audio`, … |
| `license_url` | The licence the item names |
| `rights` | The rights statement |
| `public_date` | When the item was made public on archive.org |
| `ark` | The item's ARK, for example `ark:/13960/t53f4ms96` |
| `isbn` | The ISBN-13, when every valid ISBN the item lists is the same book |
| `openlibrary` | The Open Library edition (`OL…M`), when the item names one and its older `openlibrary` field does not name another |
| `openlibrary_work` | The Open Library work (`OL…W`), when the item names one |
| `doi` | The DOI, in lower case, when the item names one |
| `page_url` | The item's page on archive.org |

## Keys and groups

The source records no keys. An item's ISBN, Open Library edition and DOI are what its uploader typed:
they stay facts and join nothing.

## Credit

Each item states its own licence or rights, which the `attribution` line carries.

## Turn it off

```toml
[sources.internet_archive]
enabled = false
```

Internet Archive links then get ordinary web cards, and `ia:` roots wait until the source is on
again.

## Examples

- `https://archive.org/details/bitcoin-whitepaper` (a `Book`)
