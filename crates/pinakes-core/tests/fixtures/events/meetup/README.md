# Meetup recordings

Public Meetup answers recorded on 2026-11-14 for `tests/meetup_series.rs`,
trimmed to what Pinakes reads:

| File | Recorded from |
| --- | --- |
| `316908442.html`, `316908445.html` | `https://www.meetup.com/bitcoin-meetup-switzerland/events/{id}/` |
| `frxnbvyjcqbmb.html` | the same date as `316908442`, under its token: `…/events/frxnbvyjcqbmb/` |
| `316908442.ics`, `316908445.ics` | `…/events/{id}/ical/{id}.ics`, the date's own calendar file |
| `frxnbvyjcqbmb.ics` | `…/events/frxnbvyjcqbmb/ical/frxnbvyjcqbmb.ics` (it names the number's UID) |
| `group.ics` | `…/events/ical/`, the group's calendar (ten dates) |

Kept: the head canonical, `og:` URL and title, the JSON-LD Event and site Organization, and the
page state's Event, Venue and Group records (id, token, title, event URL, times, series text,
time zone, venue, group name and link). Removed: hosts, attendees, RSVPs, member records,
photos, topics and every script but the page state; descriptions are cut to one sentence. The
calendars are unfolded, with each `DESCRIPTION` cut the same way. The test moves every date by
whole 52-week steps so the first one is ahead of the day it runs.
