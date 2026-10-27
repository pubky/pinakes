Flathub v2 API answers (`flathub.org/api/v2/appstream/{id}`), recorded once on 2026-10-26 with
the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`). Parser regression fixtures for
`pinakes_core::sources::flathub`; test runs make no requests.

An unknown app answers 404 `{"detail":"App not found"}` (`appstream-missing.json`).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The metadata is the app's own AppStream file.
