# `musicbrainz_containment`: a recording's releases

One of the sources of [also on](../api.md#also-on), enabled by default. It adds facts,
not identity links: a `MusicRecording` with exactly one verified `mb:recording` key
gets up to eight `track_of_album` values, each the MusicBrainz URL of an official
release containing that exact recording. No release key, barcode or album service
link is copied into the recording's group.

The source browses one page of at most 100 releases. An incomplete read retries
rather than replacing known containment with an empty result. Results expire after
one day; requests share MusicBrainz's host pace and discovery cache. More than eight
release IDs are capped and reported in the log.

`[also_on.sources.musicbrainz_containment] enabled = false` disables the source.
