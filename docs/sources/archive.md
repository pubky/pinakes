# `archive`: an archived copy of a gone page

A lookup family of [also on](../api.md#also-on) (`lookups/archive.rs`).

`wayback`: a page that is `missing` or `unreachable` gets its Wayback
Machine capture closest to when it was first referenced (captures that answered 200 only), with
`kind: "archived"` and the capture time in `at`. archive.today is never used.

## Operations

Each gone page costs one Wayback CDX request. The Wayback Machine is paced at one request every
2 s (its CDX API asks for it; the pace covers the host, so a tagged capture's card waits behind
the lookups too).
