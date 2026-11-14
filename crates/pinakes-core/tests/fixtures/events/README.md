# Event fixtures and provenance

`vendors.ics` and the inline page/calendar fixtures in `tests/events.rs` are synthetic
MIT-licensed fixtures authored for this repository; they contain no vendor attendee data.

`meetup/` holds trimmed public Meetup answers for `tests/meetup_series.rs`; its README says
where each came from and what was removed.

The calendar pitfalls the tests cover:

| Pitfalls | Regression coverage |
| --- | --- |
| folding, escapes, quoted/caret parameters | vendors fixture |
| floating/Google UTC masters/DATE/DURATION/Windows/custom/IANA precedence | vendor, timezone and weekly tests |
| rule constraints, WKST, BYSETPOS, invalid dates, DST, mixed RDATE/EXDATE, overrides/range/orphans/split UID | recurrence and remaining-pitfalls tests |
| cancellation/disappearance, clocks/revisions, calendar names | lifecycle, identity and vendor tests |
| GEO/Apple/VLOCATION, conference detection and image restrictions | vendor and remaining-pitfalls tests |
| URL/empty URL, repeated escaped categories, duplicate UIDs, ignored vendor noise | identity, category and vendor tests |
| caps, GET/sniffing, validators, horizon, occurrence limits | transport/protocol/recurrence/freeze tests |
| canonical feed and scoped UID identity | identity helper and occurrence-key tests |

JSON-LD tests include graphs/arrays/subtypes, lenient control/trailing-comma recovery,
PostalAddress, date-only end, offset without colon, missing fields and online offers.
ActivityPub tests include both Accept forms, one redirect, Mobilizon fields and a
Gathio Person falling through to an ICS. Integration tests use disposable Postgres,
mock Nominatim and synthetic Pubky event lines; no public service calls in the suite.
