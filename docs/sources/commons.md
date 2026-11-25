# Wikimedia Commons

Pinakes reads media files on Wikimedia Commons (photos, drawings, video, audio, documents) from the
Commons MediaWiki API, by file name. No key is needed.

| | |
| --- | --- |
| Source | `commons`, official source |
| Links | `commons:File:{Name}`; file description pages on `commons.wikimedia.org` and `commons.m.wikimedia.org` |
| Fetches | `GET https://commons.wikimedia.org/w/api.php?action=query&prop=imageinfo\|coordinates&titles=File:{name}` (in full [below](#what-it-fetches)) |
| Credentials | None |
| Rate | At most 200 requests a minute on `commons.wikimedia.org` (Wikimedia's published limit); the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower. Every source that asks this host shares the rate |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `ImageObject`, `VideoObject`, `AudioObject`, `DigitalDocument`, `MediaObject` |
| Keys | None (facts only) |
| Credit | The file's attribution text (else its author) and its licence, in `attribution` |
| Switch | `[sources.commons] enabled`, on by default |

## Links

Every address of one file's description page is one resource. The root is `commons:File:{Name}`, and
these links are [aliases](../api.md#aliases) of it:

| Link | Notes |
| --- | --- |
| `https://commons.wikimedia.org/wiki/File:{name}` | Also on the mobile host `commons.m.wikimedia.org` |
| `https://commons.wikimedia.org/w/index.php?title=File:{name}` | With other parameters such as `action=history` |
| `https://commons.wikimedia.org/w/api.php?…&titles=File:{name}` | One title |
| `https://commons.wikimedia.org/w/index.php?curid={page id}`, `…/w/api.php?…&pageids={page id}` | The root is the file name the answer gives |

In a link, the name can have spaces or underscores and can be percent-encoded. The namespace can be
`File:` or its alias `Image:`, in any case.

In the root, the name is in MediaWiki's normal form: underscores for spaces, runs of them collapsed,
and the first letter in upper case, for example
`commons:File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg`. Letters outside ASCII are
percent-encoded (`commons:File:%C3%89t%C3%A9.jpg`), and so is a `"` after the name's first `?`.
A `commons:` root that is not in this form is `missing` at once and never fetched.

A renamed file answers through MediaWiki's redirect. The card's root is then the new name, and the
old name is an alias of it.

Not read here:

- the media file itself (`upload.wikimedia.org`);
- `File:` pages on other wikis;
- a past revision of a page (`oldid` or `diff`);
- a link that gives its title or page ID twice;
- other pages of Commons, such as categories, galleries and `Special:FilePath`.

## What it fetches

One request per read:
`GET https://commons.wikimedia.org/w/api.php?action=query&format=json&formatversion=2&redirects=1&prop=imageinfo|coordinates&iiprop=url|size|mime|extmetadata|timestamp&iiurlwidth=1280&iiextmetadatafilter=…&iiextmetadatalanguage=en&titles=File:{name}`,
or `pageids={page id}` in place of `titles`.

| Answer | What happens |
| --- | --- |
| A file Commons does not have, or a page ID that is not a file's page | `missing` |
| The API error `ratelimited`, `maxlag` or `readonly` | Read again later |
| Any other API error, or an answer for another file | An error, and the read is retried |

## Card

The type follows the file's MIME type: `ImageObject`, `VideoObject`, `AudioObject`,
`DigitalDocument` for a PDF, else `MediaObject`.

- `site` is "Wikimedia Commons".
- `name` is the file's object name, else its file name without the extension.
- `description` is the plain text of its English description.
- `image` is a 1280-pixel rendering, or the file itself when it is a JPEG, PNG, GIF or WebP at most
  1280 pixels wide.
- `location` is the file's primary coordinates, when they are on Earth.
- `attribution` is the file's attribution text (else its author) and its licence, for example
  "© Frank Schulenburg / CC BY-SA 4.0".
- The file's description page is the `page_url` fact. Because the root `commons:File:{Name}` is not
  a web link, the API also lists that page in `open_in`.

| Fact | Meaning |
| --- | --- |
| `author` | The author, as plain text |
| `credit` | The source line, for example "Own work" |
| `license_short_name` | For example `CC BY-SA 4.0`, `Public domain` |
| `license_url` | The licence's URL |
| `usage_terms` | The licence's full name |
| `attribution_required` | `true` or `false` |
| `date_original` | When the photo was taken, as the file gives it |
| `mime` | The file's MIME type |
| `width`, `height` | In pixels |
| `size` | In bytes |
| `page_id` | The file page's ID (the `?curid=` number) |
| `page_url` | The file's page on Commons |

## Credit

Every file has its own author and licence. The `attribution` line credits the file's attribution
text, else its author, with its licence. The facts `author` and `credit` (the source line) and the
licence facts `license_short_name`, `license_url` and `usage_terms` hold what Commons states for
the file. The attribution text itself is not a fact.

## Turn it off

```toml
[sources.commons]
enabled = false
```

Commons links then get ordinary web cards, and `commons:` roots wait until the source is on again.
Images that other cards take from Commons are not affected.

## Examples

- `https://commons.wikimedia.org/wiki/File:Golden_Gate_Bridge_as_seen_from_Battery_East.jpg` (an
  `ImageObject` with a location)
- `https://commons.wikimedia.org/wiki/File:Bitcoin.svg` (an `ImageObject`)
