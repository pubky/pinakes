# `editions`: which release each link of a recording or an album is on

One of the music sources of [also on](../api.md#also-on): links on music services are looked up exactly, never by title or name.

`editions` runs last, after every source that finds links. For a track or album card with a
MusicBrainz key (`mb:recording`, `mb:release`, `mb:release-group`), it places the links on other
services that the card may show (its own, its group's members and their derived links) on
MusicBrainz releases, so the card's `also_on` shows, of each service, the link on the card's own
release, else on the original edition.

- **The original release group** of a recording is the first of those MusicBrainz lists its
  official releases in by type: an album, then a single, then an EP, then any other; one with no
  secondary type (a compilation, a soundtrack, a live album) before one with; then the earliest
  first release. An album's is its own.
- **The original edition** is that group's official releases whose disambiguation names no later
  edition (deluxe, anniversary, remastered, expanded, reissue, bonus tracks, a box set, hi-res such
  as `24-bit` or `96 kHz`), digital ones first, then worldwide ones, then the earliest. A store's
  own copy of the original (`Apple Digital Master`, `Mastered for iTunes`) is no later edition.
- **The card's own release:** for a track, the release(s) MusicBrainz relates the album page of
  the card's own track to (Spotify's track page names its album, iTunes and TIDAL their track's,
  Deezer's while its source is on); for an album, the release(s) its link is a page of.
- **Placing a link:** the source reads the album pages MusicBrainz relates to the card's own
  release (rank 0) and to the original edition's releases (rank 1, 2, … in that order). A track
  link is on such an album when its service lists it among the album's tracks: Apple Music
  through iTunes (`lookup?id={album}&entity=song`), Spotify by its album page's `music:song`
  tags, TIDAL by its API (with the operator's credentials) and Deezer by its API (while its
  source is on). An album link is on the release whose album pages name it. Qobuz, YouTube and
  the other services have no track list here: their links stay unplaced, and the card's order
  among them is unchanged.

The places are stored per card (`link_release`) and live as the source's other results do. A link
placed on no release is shown only where no link of its service is placed.

## Operations

Per card: up to four pages of a recording's releases (one for an album's release group, at most
two), up to six releases' URL relations, the card's own album (one page or lookup, and a
MusicBrainz URL lookup) and up to two track lists per service; answers are shared for an hour,
so the tracks of one album ask once. Nothing is read while a card has no link to place. After an
upgrade, `pinakesd reextract also-on` places the links of every card already stored.
