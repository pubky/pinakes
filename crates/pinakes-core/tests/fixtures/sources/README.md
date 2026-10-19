Responses from the official APIs that `pinakes_core::sources` reads, one directory per source
(`<name>/`, named as in `[sources.<name>]`). Each was recorded once with the Pinakes User-Agent
and the contact `info@synonym.to`, at the service's published pace; test runs make no public
requests.

Each directory's `provenance.json` lists, per fixture, the request URL (and method and body for a
POST), the HTTP status, the content type, the time of recording, the size and SHA-256 of the body
as served, and the file that holds it. Bodies over 64 KB are stored gzip-compressed (`.gz`); a
redirect is recorded as its status and `Location` only; an error page keeps the extension of its
content type. Licences and required attributions are named in `docs/sources/<name>.md`.
