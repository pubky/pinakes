ClinicalTrials.gov API (v2) responses, recorded once on 2026-10-19 with the Pinakes user agent
(`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every 1.5 seconds,
as searches by number (`filter.ids`) with `fields=protocolSection,hasResults` (the card's
request; the full records are megabytes). `search-NCT04482322` asks for a former number and is
answered with the study it was merged into; `search-NCT99999999` is a number never assigned.
Parser regression fixtures for `pinakes_core::sources::clinicaltrials`; test runs make no
requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Bodies over 64 KB are stored gzip-compressed (`.gz`).
The data is a U.S. government registry (National Library of Medicine), in the public domain;
ClinicalTrials.gov asks to be credited with the date the data was retrieved or last updated.
