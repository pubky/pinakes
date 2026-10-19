# Recorded adapter regression inputs

Public provider responses captured on 2026-10-16, reused without repeat HTTP calls.
`responses.json` preserves available capture time/status/headers and
body SHA-256; bodies are gzip-compressed with deterministic metadata. The captured
pages are test input, never executed or served. Source content rights remain with
the respective publishers. API credentials and authenticated content are excluded.
Outlook, iCloud and Nextcloud calendar URLs are synthetic parser fixtures; their
canaries are disabled. `scripts/verify-no-capability-urls` checks these files and
compressed bodies without printing matched capabilities.

`cases.json` lists 45 keyless probes and expected type/status/fields on these inputs.
The integration test uses mock DNS/transport and disables relay fallback. Thirty-eight
produce the expected fields. Billetto/Rausgegangen/Reservix return 403, Dice returns
503; Guidle's calendar discovery is refused by the egress content-type policy (the
failure propagates). Fever exceeds the page cap, and the selected Entradium page lacks event fields.
These failures are evidence of canary behavior, not claims of successful extraction.
Counts of time-windowed occurrences are deliberately not fixed as the clock advances.

The other adapters are covered by synthesized documented contracts in adapters.rs:
keyed APIs have no configured credentials, Proton has no public share sample, and
Class C wall probes are optional. This fixture set also includes
reusable captured discovery responses and the generic-calendar sample. Google,
iCloud, Outlook, Nextcloud and Starticket inputs exercise the shared feed parser.
