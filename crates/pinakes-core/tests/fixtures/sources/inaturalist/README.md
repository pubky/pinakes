iNaturalist API (v1) responses from api.inaturalist.org, recorded once on 2026-10-19 with the
Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), at least 2.5 seconds
apart (the API asks for at most 60 requests a minute). Parser regression fixtures for
`pinakes_core::sources::inaturalist`; test runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Bodies over 64 KB are stored gzip-compressed (`.gz`).
The licence is per observation and per photo: observation 333069440 and its photo are CC BY-NC
(ramchop), observation 195404405 and its photos CC BY (Michael Berardozzi). The taxon's default
photo is "all rights reserved"; its Wikipedia summary is CC BY-SA 4.0. `sites.json` is the list of
iNaturalist Network sites.
