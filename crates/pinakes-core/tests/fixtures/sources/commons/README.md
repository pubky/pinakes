Wikimedia Commons MediaWiki API responses (`action=query` with `prop=imageinfo|coordinates`, and
one `wbgetentities` structured-data answer), recorded once on 2026-10-19 with the Pinakes user
agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), one request at a time, at least
2 s apart. Parser regression fixtures for `pinakes_core::sources::commons`; test runs make no
requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. `redirect.json` is a renamed file
(`File:Bitcoin BTC symbol.svg` → `File:BitcoinSign.svg`); `curid-*` are requests by page id.
File description text is CC BY-SA 4.0 (Wikimedia Commons); structured data is CC0; each file has
its own licence, named in its `extmetadata` (`LicenseShortName`, `LicenseUrl`, `Artist`).
