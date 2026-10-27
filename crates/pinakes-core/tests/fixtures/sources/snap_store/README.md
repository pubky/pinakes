snapcraft.io v2 info API answers (`api.snapcraft.io/v2/snaps/info/{name}?fields=…`), recorded
once on 2026-10-26 with the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`) and the
header `Snap-Device-Series: 16` the API asks for. Parser regression fixtures for
`pinakes_core::sources::snap_store`; test runs make no requests.

An unknown snap answers 404 with `resource-not-found` (`info-missing.json`).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The metadata is the publisher's own.
