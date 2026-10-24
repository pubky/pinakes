# `service`: a music service's own ISRC

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`service`: the ISRC a music service's own track page states (its JSON-LD `isrcCode`), as the
key `isrc:…` (`via` the service) only from a service whose catalogue comes from labels and whose
pages were checked to state it: TIDAL. Elsewhere it is the fact `isrc` (on Bandcamp and
SoundCloud an uploader types it), and not when MusicBrainz gives that ISRC to several recordings
(it names none of them). A track whose ISRC nobody else knows (not MusicBrainz,
not another link) stays alone with its key, and joins as soon as another link or MusicBrainz
carries the same ISRC.
