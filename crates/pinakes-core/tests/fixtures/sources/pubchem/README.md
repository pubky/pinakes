PubChem PUG REST responses (`pubchem.ncbi.nlm.nih.gov/rest/pug`), recorded once on 2026-10-19 with
the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request
every 1.5 seconds (the service allows 5 a second and reports its load in `X-Throttling-Control`).
Parser regression fixtures for `pinakes_core::sources::pubchem`; test runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. `compound-missing` is the property table of a CID never
assigned: 200 with a row holding only the CID. `record-missing` and `name-missing` are 404s with a
PUG REST `Fault`.
PubChem data is public; NCBI asks users to credit PubChem
(https://www.ncbi.nlm.nih.gov/home/about/policies/).
