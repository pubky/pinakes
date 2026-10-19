Hugging Face Hub API responses, recorded once on 2026-10-19 with the Pinakes user agent
(`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request a second (the Hub
allows 500 API requests per 5 minutes). Parser regression fixtures for
`pinakes_core::sources::huggingface`; test runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served; the redirects (307) also carry their `Location`. The
records are repo metadata the Hub publishes; each repo's content has its own licence (the
`license` detail), and use of the Hub falls under its terms of service.
