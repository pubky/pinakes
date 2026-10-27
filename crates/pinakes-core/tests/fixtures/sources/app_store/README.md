iTunes Lookup API answers (`itunes.apple.com/lookup?id={id}`), recorded once on 2026-10-26 with
the Pinakes user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`). Parser regression fixtures for
`pinakes_core::sources::app_store`; test runs make no requests.

The API answers `text/javascript` holding JSON; the files keep the `.json` extension. An unknown
ID answers 200 with `{"resultCount":0,"results":[]}` (`lookup-missing.json`, ID 1).

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The descriptions and icons are the developers' and
Apple's, shown under the App Store's terms.
