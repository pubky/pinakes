Art Institute of Chicago API responses, recorded once on 2026-10-19 with the Pinakes user agent
and the `AIC-User-Agent` header the API asks for (both
`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every two seconds (the
API allows 60 per minute). Parser regression fixtures for `pinakes_core::sources::artic`; test runs
make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. As each response's `info.license_text` says, the
`description` field is CC BY 4.0 and all other data is CC0 1.0 (and the Terms and Conditions of
artic.edu apply); images of public-domain works are CC0.
