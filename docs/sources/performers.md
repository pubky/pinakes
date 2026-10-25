# `performers`: an event's performers, keyed to the artist on the page's own service

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`performers`: an event page (`MusicEvent`, or any event) that names its performers
in its own structured data (schema.org `performer`) has each performer's name as the fact
`performer`. The artist's identity is the key `mb:artist:…` only where the page speaks for the
service of a link it gives for the performer (`url`, `sameAs`): a Songkick concert naming a
Songkick artist page, a Bandsintown event naming a Bandsintown artist page, a music service's own
event page naming that service's artist page, when MusicBrainz relates the link (in the spelling
MusicBrainz stores: Songkick and Bandsintown artists by number) to exactly one artist; that MBID is
also the fact `performer_mbid`. The page is the one the fetch ended on, after redirects (a Songkick
address that redirects off Songkick is the other site's page); a stored card that does not record
it keys nothing until its page is read again. Any other page chose the identity it names, so it is
a fact at most: the artist MusicBrainz relates one of its links to, else a MusicBrainz artist page
it names, is the fact `performer_mbid`, never a key. A performer without either keeps its first
link as the fact `performer_link`. A name alone is never a key, and nothing is searched by name; a
performer the page does not key is facts only (a read's `facts`), not found by `key=`.
Text search (`q=`) matches an event's name and description, not its performers: an act the page
names only in `performer` (a festival's support act) is not found by it.
Performers are read from the page's own event node only: one whose `url` (else its `@id`) names
the page, tracking parameters and fragments aside, or one naming no page in a head script or as
the main entity of the page's own node; never from another event the page describes, and never
because a sibling node names the page (a body script's event naming no page gives none). Among
events the page's own is read first. A page keeps up to 16 performers with 8 links each, within
2 KB. Events [group](../api.md#groups) only with events, so the key never joins a concert to the
artist's pages; it makes the concert the artist's event:
`GET /v1/search?key=mbid:{artist}&type=MusicEvent&from=now&sort=start` lists an artist's upcoming
concerts, soonest first.

## Operations

Each event page naming performers (up to 16, with 8 links each, within 2 KB of the document)
costs one MusicBrainz URL request for all its performers' readable links together and no other
(an artist's page on a music service, Songkick or Bandsintown; at most two per performer; a link
the answer leaves out is unknown until the next run, and a busy or failed request fails the run of
a page on a performer link's service, which retries, while another page records what it says
without MusicBrainz and asks again after MusicBrainz's retry time; answers are cached an hour, so
an artist's many concerts naming only that artist, by one link, ask once), and none for a
performer the page names by a MusicBrainz artist page alone. `[also_on.sources.musicbrainz]
base_url` points its reads at a MusicBrainz mirror, as for the other MusicBrainz sources.
