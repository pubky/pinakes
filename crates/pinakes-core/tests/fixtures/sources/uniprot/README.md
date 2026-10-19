UniProt REST API responses (`rest.uniprot.org`), recorded once on 2026-10-19 with the Pinakes
user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every 1.5
seconds, release 2026_03. Parser regression fixtures for `pinakes_core::sources::uniprot`; test
runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Entries are requested with a `fields` list (the full
P04637 record is 875 KB). `entry-q15086-merged` is a 303 (redirects not followed) whose body is
the inactive record; `entry-p00001-demerged` and `entry-o51290-deleted` are inactive records;
`entry-missing` is a valid accession never assigned. `name-*` are entry-name searches:
`name-nope-human-former` shows that a former entry name still finds the renamed entry.
The data is © UniProt Consortium, CC BY 4.0 (https://www.uniprot.org/help/license).
