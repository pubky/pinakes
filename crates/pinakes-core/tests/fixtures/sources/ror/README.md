Research Organization Registry v2 API responses (`api.ror.org/v2/organizations/{id}`), recorded
once on 2026-10-19 with the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`),
about one request every two seconds. Parser regression fixtures for `pinakes_core::sources::ror`;
test runs make no requests.

`org-missing.json` asks for `0zzzzzz02`, a well-formed id with a valid check that names no
organization: 404 `{"errors":["ROR ID 'https://ror.org/0zzzzzz02' does not exist"]}`. An id with a
wrong check answers the same.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The data is CC0 1.0.
