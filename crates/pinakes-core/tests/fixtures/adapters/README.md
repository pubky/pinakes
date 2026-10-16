# Recorded adapter regression inputs

Public provider responses captured on 2026-10-16, reused without repeat HTTP calls.
`responses.json` preserves each source URL, available capture time/status/headers and
body SHA-256; bodies are gzip-compressed with deterministic metadata. The captured
pages are test input, never executed or served. Source content rights remain with
the respective publishers. API credentials and authenticated content are excluded.

`cases.json` lists 45 keyless probes and expected type/status/fields on these inputs.
The integration test uses mock DNS/transport and disables relay fallback. Thirty-nine
produce the expected fields. Billetto/Rausgegangen/Reservix return 403, Dice returns
503, Fever exceeds the page cap, and the selected Entradium page lacks event fields.
These failures are evidence of canary behavior, not claims of successful extraction.
Counts of time-windowed occurrences are deliberately not fixed as the clock advances.

The other adapters are covered by synthesized documented contracts in adapters.rs:
keyed APIs have no configured credentials, Proton has no public share sample, and
Class C wall probes are optional per research §G. This fixture set also includes
reusable captured discovery responses and the generic-calendar sample. Google,
iCloud, Outlook, Nextcloud and Starticket inputs exercise the shared feed parser.
