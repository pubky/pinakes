UniProt REST API responses (`rest.uniprot.org`), recorded once on 2026-10-19 with the Pinakes
user agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every 1.5
seconds, release 2026_03. Parser regression fixtures for `pinakes_core::sources::uniprot`; test
runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. `search-*` are searches by accession with the card's
`fields` list: an active entry (P69905, P04637), a merged accession (Q15086, naming P04637), a
demerged one (P00001), a deleted one (O51290) and an accession never assigned (P0DZZ9, no
results). `name-*` are entry-name searches: `name-nope-human-former` shows that a former entry
name still finds the renamed entry. The data is © UniProt Consortium, CC BY 4.0
(https://www.uniprot.org/help/license).
