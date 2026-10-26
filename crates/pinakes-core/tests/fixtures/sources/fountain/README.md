Fountain show and episode pages, recorded once on 2026-10-26 with the Pinakes user agent
(`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), for `pinakes_core::sources::fountain`'s
page hook; test runs make no requests. The pages are gzip-compressed, as served apart from that.

The show page is TFTC: A Bitcoin Podcast (`/show/V71wG3DhMtF9fDqaJRWX`), and the episode one of
its items (`/episode/Yj4aqLaHXcVmxgXbMLHb`). Both carry Fountain's site-wide Organization and
WebSite JSON-LD in the head. Their title and og: tags are streamed as children of `<body>`, and
the app data embedded in their scripts names the show's feed (`rss`) and each record's `_id`,
`_guid` and `_type`.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. The pages and their listing are Fountain's and the
podcast's publisher's.
