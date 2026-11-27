# Recorded adapter regression inputs

Public provider responses captured on 2026-10-16 (Guidle's and Infomaniak's on 2026-10-30),
reused without repeat HTTP calls.
`responses.json` preserves available capture time/status/headers and
body SHA-256; bodies are gzip-compressed with deterministic metadata. The captured
pages are test input, never executed or served. Source content rights remain with
the respective publishers. API credentials and authenticated content are excluded; the keys
a page's publisher sends every visitor (map API keys, presigned-link credentials) are replaced
by `REDACTED` (`scripts/scrub-publisher-keys`), and a scrubbed body's name, `sha256` and `bytes`
are its scrubbed form's; its row has no `blake3`, which named the body as recorded.
Outlook, iCloud and Nextcloud calendar URLs are synthetic parser fixtures; their
canaries are disabled. `scripts/verify-no-capability-urls` checks these files and
compressed bodies without printing matched capabilities.

`cases.json` lists 45 keyless probes and expected type/status/fields on these inputs.
The integration test uses mock DNS/transport and disables relay fallback. Thirty-nine
produce the expected fields. Billetto/Rausgegangen/Reservix return 403, Dice returns
503. Fever exceeds the page cap, and the Entradium capture is its "update your browser" page.
Billetto, Rausgegangen, Reservix, Entradium and Starticket are off now, so their links are read
as plain web pages. Infomaniak's case is its canary: the listing, then its first event page.
These cases test how failures surface; they do not show extraction working.
Counts of time-windowed occurrences are not fixed, since the clock advances.

The other adapters are covered by synthesized documented contracts in adapters.rs:
keyed APIs have no configured credentials, Proton has no public share sample, and
Class C wall probes are optional. This fixture set also includes
reusable captured discovery responses and the generic-calendar sample. Google,
iCloud, Outlook, Nextcloud and Starticket inputs exercise the shared feed parser.
