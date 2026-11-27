Responses from external APIs, recorded once on 2026-10-19 with the Pinakes user agent and the
contact `info@synonym.to`, at each service's published pace (a later one says when and how in its
`provenance.json` entry). They are parser regression fixtures for `pinakes_core::lookups` and the
DOI card (`papers/crossref-csl-*`); test runs make no public requests.

`provenance.json` lists, for each fixture, the request URL, the HTTP status, the content type,
the time of recording, the size and SHA-256 of the body as served, and the file that holds it.
Bodies over 64 KB are stored gzip-compressed (`.gz`). A redirect is recorded as its status and
`Location` only. Some 404 answers are HTML pages and are stored as `.html`.

Licences of the recorded data: Wikidata, DataCite and Crossref CC0; OpenStreetMap (through Nominatim)
© OpenStreetMap contributors, ODbL 1.0; ProtonDB ODbL. Other responses are public API metadata, kept solely as private test fixtures.
