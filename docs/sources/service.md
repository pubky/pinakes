# `service`: a music service's own ISRC

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`service`: the ISRC a music service's own track page states (its JSON-LD `isrcCode`), as the
key `isrc:…` (`via` the service) only from a service whose catalogue comes from labels and whose
pages were checked to state it: TIDAL. Elsewhere it is the fact `isrc` (on Bandcamp and
SoundCloud an uploader types it). An ISRC MusicBrainz gives to several recordings names none of
them by itself: the page's duration decides. The one recording whose length is within 3 s of it,
while every other's length is known and is not, is the track's: its MBID is the key
(`mb:recording:…`, `via` the service) and the ISRC a fact. When none or two are that close, or
the page states no duration, the ISRC is a fact only (a key would join whichever recording's
group took it first). A track whose ISRC nobody else knows (not MusicBrainz, not another link)
stays alone with its key, and joins as soon as another link or MusicBrainz carries the same ISRC.
