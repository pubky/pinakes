# Event fixtures and provenance

`vendors.ics` and the inline page/calendar fixtures in `tests/events.rs` are synthetic
MIT-licensed fixtures authored for this repository. They exercise shapes documented
in the supplied `research-event-sources.md`, without copying another workspace's
scratchpad or vendor attendee data. Public live probes and hashes are in the 4A handoff;
raw probe responses are kept only in the ignored `.runtime/events-live*` directories.

Research §F coverage (numbers refer to the supplied research):

| Pitfalls | Regression coverage |
| --- | --- |
| 1–3 folding, escapes, quoted/caret parameters | vendors fixture |
| 4–8 floating/Google UTC masters/DATE/DURATION/Windows/custom/IANA precedence | vendor, timezone and weekly tests |
| 9–13 rule constraints, WKST, BYSETPOS, invalid dates, DST, mixed RDATE/EXDATE, overrides/range/orphans/split UID | recurrence and remaining-pitfalls tests |
| 14–16 cancellation/disappearance, clocks/revisions, calendar names | lifecycle, identity and vendor tests |
| 17–19 GEO/Apple/VLOCATION, conference detection and image restrictions | vendor and remaining-pitfalls tests |
| 20–22 URL/empty URL, repeated escaped categories, duplicate UIDs, ignored vendor noise | identity, category and vendor tests |
| 23 caps, GET/sniffing, validators, horizon, occurrence limits | transport/protocol/recurrence/freeze tests |
| 24 canonical feed and scoped UID identity | identity helper and occurrence-key tests |

JSON-LD tests include graphs/arrays/subtypes, lenient control/trailing-comma recovery,
PostalAddress, date-only end, offset without colon, missing fields and online offers.
ActivityPub tests include both Accept forms, one redirect, Mobilizon fields and a
Gathio Person falling through to an ICS. Integration tests use disposable Postgres,
mock Nominatim and synthetic Pubky event lines; no public service calls in the suite.
