# `musicbrainz_entity`: a MusicBrainz page as its own hub

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`musicbrainz_entity`: a MusicBrainz recording, release, release group or artist page ([its
spellings](../api.md#aliases)) is its own hub, read as `musicbrainz` reads the entity a service
link is related to: its MBIDs, the recording's ISRCs and the release's barcode as keys (so it joins
the links MusicBrainz relates to it), its facts, and its current links on music services at its
level, each keyed by the entity. An ISRC's page (`/isrc/{ISRC}`) is the ISRC's key, and with
exactly one recording MusicBrainz gives the ISRC to, that recording's keys, facts and links. An
ISRC several recordings have names none of them: its page keeps it as a fact only (keyed, it would
join whichever of their groups took it first, the other refused). A page whose MBID MusicBrainz
merged into another entity keys that entity and keeps its own MBID as the fact `mbid_was`, so
`key=mbid:{old}` still finds it. An artist's or release group's page also keys the Wikidata item
its own current `wikidata` relation names (`wikidata:{Q-id}`, the exact item link; relations that
name several items, or no item, key none) once that item, read back, is exactly an artist's item
for an artist or a release group's for a release group, naming this one MBID (`P434`, `P436`) and,
for a release group, no recording (`P4404`): an item that is a work's too or names a recording (a
single's song's or track's item, which a track can be keyed by through its streaming IDs), one of
another kind (an album's relation to its artist's item) or one with no identifier yet keys nothing,
though the card's enrichment may still take its text. So it joins a link Wikidata names for that
item; a page whose item differs from another member's stays out of that group (one item per group,
[groups](../api.md#groups)). A recording's or release's page keys no item: MusicBrainz relates
items to artists and release groups, and an item a recording or release named would be its song's
or its album's, another level's.

## Operations

Each MusicBrainz page costs one request for its entity (an ISRC's page one more for the ISRC;
MusicBrainz has no lookup of several entities at once, and a read the card already made is taken
from the cache) and one for a recording's original album; an artist's or release group's page with
a `wikidata` relation one Wikidata read of that item (shared with its card's enrichment through the
cache). A Wikidata that fails for now leaves the MusicBrainz keys and links standing, keeps the
item key the page had, and brings the run back after its wait.
