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
- **The original edition** is that group's official releases that name no later edition
  (deluxe, legacy, anniversary, remastered, expanded, reissue, special or collector's edition,
  bonus tracks, a box set, hi-res such as `24-bit` or `96 kHz`) in their disambiguation or in their
  title past the group's (`… (Deluxe Edition)`); "edition" alone (standard, original, explicit)
  names none, and a store's own copy of the original (`Apple Digital Master`, `Mastered for
  iTunes`) is no later edition. The earliest first; of one date, digital ones, then worldwide
  ones.
- **The card's own release:** for a track, the release(s) MusicBrainz relates the album page of
  the album its own service names to (below); for an album, the release(s) its link is a page of.
- **Ranks:** the album pages MusicBrainz relates to the card's own release rank 0, then those of
  the original edition's releases 1, 2, … in that order. So a service's link lands on its earliest
  original release that has a page there: a 1975 release with a Spotify page before a 2011 one,
  and a later digital release only when no earlier one has a page on that service. The pages come
  with the browse of the releases (their URL relations), so no release is read on its own but a
  card's own one the browse leaves out.
- **Placing a link:** a track link is on such an album when its own service names that album as
  the track's: Apple Music through iTunes (`lookup?id={track}`), Spotify by its Web API (with the
  operator's credentials) or, only with `[also_on] spotify_pages = true`, the track's public page
  (its `music:album`), TIDAL by its API (with the operator's credentials) and Deezer by its API,
  each while its source is on. An album link is on the release whose album pages name it. Qobuz,
  YouTube and the other services name no album here: their links stay unplaced, and among them the
  card's order is unchanged. Only links a card may show are read; never an album page nobody
  linked. Where Spotify's API refuses the operator's credentials and the pages are off (as on an
  instance whose Spotify app owner has no premium plan), Spotify links stay unplaced.
- **A failure** of any of these lookups (a busy or failing service) fails the run: the places
  recorded before stay, and the run comes back after its backoff. Credentials a service refuses
  are no failure: its links are not placed.
- **When a group changes** (a member joins or leaves, a link becomes another's alias), the sources
  of its music cards run again soon, so each places the links it may show now.

The places are stored per card (`link_release`) and live as the source's other results do. A link
placed on no release is shown only where no link of its service is placed.

## Operations

Per card: up to four pages of a recording's releases with their URL relations (an album's
release group: up to two), the card's own track's album (one lookup, and a MusicBrainz URL lookup;
a release of its own the browse leaves out is read), and each candidate track's album (at most
eight per service); answers are shared for an hour. Nothing is read while a card has no link a
service can place. After an upgrade, `pinakesd reextract also-on` places the links of every card
already stored.
