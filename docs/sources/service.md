# `service`: a music service's own ISRC

This source runs on every link to a music service. It records what the link's own page says about
the object: its artist, and for a track its album, length and ISRC. It asks the service nothing
itself, because it reads the stored card.

| | |
| --- | --- |
| Source | `service`, also-on source |
| Runs on | Track, album, artist, playlist and video links on Spotify, Apple Music, YouTube, Deezer, TIDAL, Qobuz, SoundCloud, Amazon Music, Bandcamp and Discogs |
| Asks | Nothing for most links; MusicBrainz (`GET /isrc/{ISRC}`) for a TIDAL track whose page states an ISRC |
| Credentials | None |
| Rate | MusicBrainz's: 1 request per second on `musicbrainz.org` |
| Rows live | 30 days; rows expire after 60 |
| Keys | `isrc:…` and `mb:recording:…`, for TIDAL tracks only |
| Links | None |
| Facts | `isrc`, `artist`, `album`, `duration` |
| Switch | `[also_on.sources.service] enabled`, on by default |

## What it does

The source reads the fields that the page's JSON-LD left on the stored card:

| Fact | Read from | On |
| --- | --- | --- |
| `isrc` | `isrcCode` | Tracks; never videos |
| `artist` | `byArtist` (up to 8 names, joined with " & ") | Any link |
| `album` | `inAlbum` | Tracks |
| `duration` | `duration`, in seconds | Tracks, when it is between 1 second and 1 day |

An ISRC that is not well formed (two letters, three letters or digits, seven digits) is left out.

## Keys, links and facts

The ISRC is always a fact. It is also a key only when the service vouches for it: a service whose
catalogue comes from labels and whose pages were checked to state it. That is TIDAL alone. On
Bandcamp and SoundCloud, for example, the uploader types the ISRC, so it stays a fact.

For a TIDAL track, the keys depend on what MusicBrainz knows about the ISRC
([an ISRC on several recordings](README.md#an-isrc-on-several-recordings)):

| MusicBrainz's answer | Keys (`via` `tidal`) |
| --- | --- |
| No recording, or exactly one | `isrc:…` |
| Several recordings that agree | `isrc:…` and `mb:recording:…` of the one that stands for them |
| Several that disagree, and the track's length picks one (within 3 seconds, while every other recording's length is known and further away) | `mb:recording:…` of that recording, or of the one that stands for its duplicates |
| Several that disagree, and the length picks none | None |

A track whose ISRC nobody else knows keeps its key and stays alone. It joins as soon as another
link or MusicBrainz carries the same ISRC.

The source adds no links.

## Cost

No request for most links. A TIDAL track with an ISRC costs one MusicBrainz lookup. An ISRC on
several recordings also costs a read of those recordings. The answers are shared with every other
source that asks MusicBrainz the same thing.

## Turn it off

```toml
[also_on.sources.service]
enabled = false
```

Its stored keys and facts stop showing at the next start, and its cards regroup without them. A
TIDAL track can still get its ISRC from `tidal`, with the operator's TIDAL credentials.
