# `itunes`: Apple Music albums by barcode

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`itunes`: for an album with a barcode key, the Apple Music album iTunes' `lookup?upc=` names
when exactly one album answers, keyed by that barcode (so a tagged link to that album joins).

## Operations

Up to two iTunes lookups for an album with barcodes, in the storefront `[also_on]
apple_storefront` (`us`); the Apple Music links carry no storefront.
