# `itunes_song`: Apple Music's song for another service's track, by its album and place

One of the music sources of [also on](../api.md#also-on). Unlike the exact lookups, its link is a
match (the album, the place on it, the title and the length): a maybe (`kind` `maybe_same`),
never a key.

`itunes_song` (on by default; `[also_on.sources.itunes_song] enabled = false` turns it off): for a
track on Deezer, Spotify or TIDAL, or any track with an `isrc:` key, the Apple Music song at the
same place on the same album. The track's album barcode, disc and track number and length come
from:
- a Deezer track's own record (`track/{id}`: `disk_number`, `track_position`, `duration`, its
  album's `upc`), while `deezer` runs;
- a Spotify track's own record (`/v1/tracks/{id}`: `disc_number`, `track_number`, `duration_ms`,
  its album's `external_ids.upc`), while `spotify` runs with the operator's Spotify credentials;
- a TIDAL track's own record (`tracks/{id}?include=albums`: its one album and that album's
  `barcodeId`, its `duration`; its `volumeNumber` and `trackNumber` among the album's items),
  while `tidal` runs with the operator's TIDAL credentials; a track TIDAL puts on several albums
  has no one place;
- else the Deezer track Deezer's API names for the track's ISRC key (`track/isrc:…`, read back:
  its own ISRC must be the one asked), while `deezer` runs. The track's own ISRC is asked first,
  then the hubs', two at most.

A service that refuses the operator's credentials leaves the track to its ISRC on Deezer. iTunes
is then asked for the album's songs (`lookup?upc={barcode}&entity=song`, in the storefront
`[also_on] apple_storefront`), only while `itunes` runs, as `editions` asks it. The song is taken
only when the answer is one collection (iTunes answers unrelated collections for some codes:
Underclass Hero's barcode lists four, two editions of the album and two singles of its title
track, so none of its songs is taken), exactly one song is at that disc and track number, its
length (`trackTimeMillis`) is within 3 s of the track's (Deezer rounds: its Get Lucky is 367 s,
Apple's 369.6 s), and its title (`trackName`) is the track's own (Deezer's `title`, Spotify's
`name`, TIDAL's `title` with its `version`), compared as `same_recording` compares titles: a
credit such as "(feat. Pharrell Williams and Nile Rodgers)" and a remaster tag go, a version tag
such as "Live" stays. A barcode iTunes answers with one wrong collection (a single's barcode with
its album) so puts no other recording on the card. The link is
`https://music.apple.com/song/{trackId}`, `via` `itunes_song`, `kind` `maybe_same`.

## Why it is a maybe

iTunes' answers carry no ISRC, so nothing Apple says ties its song to the track's recording: the
album's barcode and the place on it are exact, the song at that place is inferred. The link is
derived but unkeyed and marked `maybe_same`: it never joins a group and is never a key of the
track, and a tagged Apple Music link stays a separate card unless MusicBrainz or Wikidata relate
it.

## Operations

Per track, while its place's source runs: one to six requests to the service that names the
place (Deezer: the track, or up to two ISRC lookups, and its album; Spotify: the track and its
album; TIDAL: the track with its albums and up to 5 pages of the album's items), each shared for
an hour with the other sources' identical requests, then one iTunes lookup at iTunes' pace (about 20 a minute). A
rate limit or an outage of any of them is retried, keeping the link an earlier run found. Switched
on after an upgrade, `pinakesd reextract also-on` asks for every stored track at once.
