USGS FDSN event service responses (`earthquake.usgs.gov/fdsnws/event/1/query?eventid=…&format=geojson`),
recorded once on 2026-10-19 with the Pinakes user agent
(`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), a few seconds apart.
Parser regression fixtures for `pinakes_core::sources::usgs`; test runs make no requests.

`event-tohoku-usp000hvnu` asks for the 2011 Tohoku earthquake by one of its non-preferred ids and
gets the same event (preferred id `official20110311054624120_30`). An unknown id answers 404 with
a plain-text error (`event-missing.txt`).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Bodies over 64 KB are stored gzip-compressed (`.gz`).
The data is public domain (U.S. Geological Survey).
