Docker Hub v2 API responses (`hub.docker.com/v2/repositories/{namespace}/{name}/`), recorded once
on 2026-10-19 with the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`),
about one request every two seconds. Parser regression fixtures for
`pinakes_core::sources::docker_hub`; test runs make no requests.

An unknown repository answers 404 `{"message":"object not found","errinfo":{}}`
(`repo-missing.json`). The API sent `x-ratelimit-limit: 180` (per minute, per IP).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The descriptions are written by each repository's owner
and shown under Docker Hub's terms; Docker states no data licence.
