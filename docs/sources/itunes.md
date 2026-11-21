# `itunes`: Apple Music albums by barcode, and a track's preview

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`itunes`: for an album with a barcode key, the Apple Music album iTunes' `lookup?upc=` names
when exactly one album answers, as a [maybe](../api.md#also-on) (`kind: "maybe"`): iTunes'
answer carries no barcode to read back, so the album is possibly the same, never a key, and a
tagged link to it does not join.

An Apple Music track's [stream](../api.md#stream) is iTunes' 30-second preview:
- iTunes' `lookup?id={track}&entity=song` must answer with that one song (never another
  track's, nor a music video's).
- Its `previewUrl` (an AAC `.m4a`, or an `.mp3`, on `audio-ssl.itunes.apple.com`, unsigned) is
  recorded as the facts `stream_url` and `stream_type`.
- The track's card takes it as `preview: true` at its next read, which the source asks for at
  once.

This works without Deezer, whose previews are signed links that expire and are never a stream.

## Operations

Up to two iTunes lookups for an album with barcodes, and one for an Apple Music track, in the
storefront `[also_on] apple_storefront` (`us`); the Apple Music links carry no storefront. A
track that store doesn't sell has no preview.
