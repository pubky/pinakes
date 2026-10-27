Google Play app pages, recorded once on 2026-10-26 with the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), for
`pinakes_core::sources::google_play`'s page hook; test runs make no requests.

`page-signal.html.gz` is Signal's page as served (1.3 MB, gzip-compressed), requested with
`&hl=en`; the mock end-to-end run serves it for the link without the parameter. Its
`SoftwareApplication` JSON-LD is past the first megabyte, in the body. An unknown package
answers 404 (`page-missing.html`).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The listing is the developer's and Google's.
