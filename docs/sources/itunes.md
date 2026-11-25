# `itunes`: Apple Music albums by barcode, and a track's preview

This source asks iTunes' lookup API for two kinds of card. An album with a barcode key gets a possibly
same Apple Music album. An Apple Music track gets iTunes' 30-second preview. No key is needed.

| | |
| --- | --- |
| Source | `itunes`, also-on source |
| Runs on | Another service's album or YouTube album playlist with a `upc:` key, and MusicBrainz release pages with one ([which cards](README.md#lookups-by-isrc-or-barcode)); Apple Music track links |
| Asks | iTunes: `GET https://itunes.apple.com/lookup?upc={barcode}&entity=album&country={storefront}` and `GET …/lookup?id={track}&entity=song&country={storefront}` |
| Credentials | None |
| Rate | About 20 requests per minute on `itunes.apple.com`, shared with every source that asks iTunes |
| Rows live | 30 days; rows expire after 60 |
| Keys | None |
| Links | `https://music.apple.com/album/{id}`, a `maybe` |
| Facts | `stream_url`, `stream_type` (Apple Music tracks) |
| Switch | `[also_on.sources.itunes] enabled`, on by default |

## What it does

Both lookups use the storefront `[also_on] apple_storefront` (`us` by default, a two-letter country
code in lower case). Apple Music links carry no storefront.

### An album by its barcode

The source asks iTunes for at most two of the album's barcode keys. It takes the Apple Music album
only when exactly one album answers, because iTunes can answer unrelated albums for a code it does
not know. Apple Music's own links are not asked.

### An Apple Music track's preview

The source asks iTunes for the track's ID. The answer must hold exactly one song with that ID, so
another track's song or a music video never counts. Its `previewUrl` must be a public link (a named
host, unsigned) to an `.m4a` file (`audio/mp4`) or an `.mp3` file (`audio/mpeg`). It becomes the card's
[stream](../api.md#stream), marked `preview: true`.

A track that the storefront does not sell has no preview.

## Keys, links and facts

The album link is a `maybe` (`kind: "maybe"`): iTunes' answer carries no barcode to read back, so the
album is possibly the same. The link is never a key, and a tagged link to that album does not join
the group.

`stream_url` and `stream_type` are the preview's link and its MIME type. A card that has no stream
yet is read again at once, and takes the preview at that read.

## Cost

Up to two iTunes lookups for an album with barcodes, and one for an Apple Music track. The answers
are shared for `limits.discovery_cache_secs` (an hour by default). A 400 or 404 means iTunes has
none. A rate limit or an outage is retried after the wait iTunes named (`itunes_busy`). Any other
status fails the run, which is retried while its stored rows stay.

## Turn it off

```toml
[also_on.sources.itunes]
enabled = false
```

Its album links stop showing at the next start, and the previews leave the cards at their next read.
`itunes_song` finds nothing while `itunes` is off, and
`editions` stops placing Apple Music tracks. Other sources still ask iTunes for their
own lookups.
