Internet Archive item metadata API responses (`archive.org/metadata/{identifier}/metadata`, plus
the full record where a rule depends on it), recorded once on 2026-10-19 with
the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), at least 2.5 s
apart. Parser regression fixtures for `pinakes_core::sources::internet_archive`; test runs make no
requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. `missing*`, `case-variant` and `dark*` are the answers for
an unknown identifier, a known identifier in the wrong case, and a taken-down item.
The metadata is published by the Internet Archive under its Terms of Use; each item states its own
licence (`licenseurl`, `rights`).
