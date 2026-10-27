F-Droid answers, recorded once on 2026-10-26 with the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`): the package API
(`f-droid.org/api/v1/packages/{package}`) and the English package page
(`f-droid.org/en/packages/{package}/`). Parser regression fixtures for
`pinakes_core::sources::fdroid`; test runs make no requests.

An unknown package answers 404 from both (`api-missing.json`, `page-missing.html`).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served.
