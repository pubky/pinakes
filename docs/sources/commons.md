# Wikimedia Commons

Media files on Wikimedia Commons (photos, drawings, video, audio), read from the Commons
MediaWiki API by file name. No key. Wikimedia asks anonymous clients for a descriptive
User-Agent, one request at a time and well under five a second; Pinakes keeps to one a second.

## Links

`commons:File:{Name}` and every address of a file's description page are one resource, with
`commons:File:{Name}` as the root. The name is in MediaWiki's normal form: underscores for
spaces, runs of them collapsed, first letter upper case (e.g.
`commons:File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg`); in the root, letters outside
ASCII are percent-encoded (`commons:File:%C3%89t%C3%A9.jpg`), as URIs are stored:

- `https://commons.wikimedia.org/wiki/File:{name}` and the mobile
  `https://commons.m.wikimedia.org/wiki/File:{name}`, with spaces or underscores, percent-encoded
  or not, and the `Image:` alias (any case of the namespace);
- `https://commons.wikimedia.org/w/index.php?title=File:{name}` (with other parameters such as
  `action=history`, but not a past revision: `oldid` or `diff` is a dated version, not read here);
- `https://commons.wikimedia.org/w/api.php?…&titles=File:{name}` (one title).

`https://commons.wikimedia.org/w/index.php?curid={page id}` is read by page id: its root is the
file name the answer gives. A renamed file answers through MediaWiki's redirect: the card's root
is the new name, and the old name is an alias of it.

The media file itself (`upload.wikimedia.org`) and `File:` pages on other wikis are not read here.
A file Commons does not have, and a page id that is not a file's page, are `missing`.

## What it reads

`GET https://commons.wikimedia.org/w/api.php?action=query&format=json&formatversion=2&redirects=1&prop=imageinfo|coordinates&iiprop=url|size|mime|extmetadata|timestamp&iiurlwidth=1280&iiextmetadatafilter=…&iiextmetadatalanguage=en&titles=File:{name}`
(or `pageids={page id}`).

## Card

`ImageObject` (`VideoObject`, `AudioObject` or `DigitalDocument` for video, audio and PDF files,
`MediaObject` otherwise), with `site` "Wikimedia Commons", `name` the file's object name (else its
name without the extension), `description` the plain text of its English description, `image` a
1280-pixel rendering (the file itself when it is a smaller JPEG, PNG, GIF or WebP), `location` the
file's primary coordinates on Earth, and `start` the day the photo was taken when the file gives a
full date (the camera's local day; the time stays in `date_original`). The attribution is the
file's credit line and licence, e.g. "© Frank Schulenburg / CC BY-SA 4.0".

| Fact | Meaning |
| --- | --- |
| `author` | The author, as plain text |
| `credit` | The source line, e.g. "Own work" |
| `license_short_name` | E.g. `CC BY-SA 4.0`, `Public domain` |
| `license_url` | The licence's URL |
| `usage_terms` | The licence's full name |
| `attribution_required` | `true` or `false` |
| `date_original` | When the photo was taken, as the file gives it |
| `mime` | The file's MIME type |
| `width`, `height` | In pixels |
| `size` | In bytes |
| `page_id` | The file page's id (the `?curid=` number) |
| `page_url` | The file's page on Commons |

## Licence

Every file has its own licence and author; the card carries them as its attribution. File
descriptions are CC BY-SA 4.0; structured data is CC0.

## Examples

- `https://commons.wikimedia.org/wiki/File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg` (an
  `ImageObject` with a location)
- `https://commons.wikimedia.org/wiki/File:Bitcoin.svg` (an `ImageObject`)
