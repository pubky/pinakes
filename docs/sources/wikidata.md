# `wikidata`: the Wikidata item of a music link

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`wikidata`: the one item carrying the link's own service ID (Spotify, Apple Music, Deezer,
TIDAL, Qobuz, YouTube, SoundCloud, Amazon Music, Bandcamp, Discogs), read back and compared
exactly (two items are ambiguous and give nothing); its other music-service IDs as links and
its Q-id and MusicBrainz ID as keys. An album item lists its Discogs master; a Discogs release
ID names an item only when the item is not an album's (no master, no release group). A track takes only an item that is one recording (one MusicBrainz
recording ID; no work, release, release group or artist ID, and not a song or single by class): a song item is a work that its studio, live and remastered
recordings share, so a track takes neither its Q-id nor its tracks. An album takes only an album's item (a MusicBrainz release group or Discogs master ID); such an item is never a track's, so no Q-id keys a track and an album. A Discogs release takes only an edition's item (a MusicBrainz release ID, or no MusicBrainz ID yet).

## Operations

Two Wikidata requests per music link (a search by its service ID cannot batch); the worker asks
Wikidata for a batch's item links together.
