# Internet Archive

Items in the Internet Archive (scanned books, films, audio, software, data and collections),
read from the item metadata API by identifier. No key; no published rate limit (Pinakes keeps to
one request every two seconds).

## Links

`ia:{identifier}` and every archive.org address of an item are one resource, with
`ia:{identifier}` as the root. Identifiers are case-sensitive and kept as written
(`Bitcoin-Whitepaper` is not `bitcoin-whitepaper`):

- `https://archive.org/details/{identifier}`, and the reader's views of it (`…/page/…`,
  `…/mode/…`, e.g. `/page/n5/mode/2up`);
- `https://archive.org/embed/{identifier}`;
- `https://archive.org/metadata/{identifier}` and `…/metadata`.

A file of an item (`https://archive.org/download/{identifier}/{file}`, or its view at
`https://archive.org/details/{identifier}/{file}`), user pages
(`/details/@name`) and the Wayback Machine (`web.archive.org`) are not items and are not read
here.

An identifier the Internet Archive does not know is `missing`, and so is a taken-down ("darked")
item.

## What it reads

`GET https://archive.org/metadata/{identifier}/metadata`: only the item's metadata record, a few
kilobytes even for an item with thousands of files.

## Card

The type follows the item's `mediatype`: `Book` (texts), `VideoObject` (movies), `AudioObject`
(audio, etree), `ImageObject` (image), `SoftwareApplication` (software), `Dataset` (data),
`WebSite` (web), `Collection` (collection), else `CreativeWork`. `site` is "Internet Archive",
`name` the title, `description` the plain text of the item's description, `image` the item's
thumbnail (`https://archive.org/services/img/{identifier}`) when its licence lets anyone show it
with a credit (CC0, the Public Domain Mark or CC BY without further terms). The item's `date`, as given (often
just a year such as `1888`), is a fact. The attribution is the
item's licence (the short name of a Creative Commons licence, else the licence URL), else its
rights statement.

| Fact | Meaning |
| --- | --- |
| `creators` | The creators, as the item names them |
| `date` | The item's date as written (`c1888`, `2008-10-31`…) |
| `publishers` | The publishers |
| `languages` | Language codes, e.g. `eng` |
| `collections` | The collections the item is in |
| `subjects` | Subjects and keywords |
| `mediatype` | `texts`, `movies`, `audio`… |
| `license_url` | The licence the uploader chose |
| `rights` | The rights statement |
| `public_date` | When the item was made public on archive.org |
| `ark` | The item's ARK, e.g. `ark:/13960/t53f4ms96` |
| `isbn` | The ISBN-13, when every ISBN the item lists is the same book |
| `openlibrary` | The Open Library edition (`OL…M`), when single and unambiguous |
| `openlibrary_work` | The Open Library work (`OL…W`) the edition belongs to, when single |
| `doi` | The DOI, in lower case, when the item names one |
| `page_url` | The item's page on archive.org |

## Licence

Item metadata is published by the Internet Archive under its Terms of Use; each item states its
own licence, which the card carries.

## Examples

- `https://archive.org/details/bitcoin-whitepaper` (a `Book`)
