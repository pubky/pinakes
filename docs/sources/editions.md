# `editions`: which release each link of a recording or an album is on

This source runs on track and album cards that carry a MusicBrainz key. It places the links on
other services that the card may show on MusicBrainz releases, so that the card's
[`also_on`](../api.md#also-on) shows, for each service, the link on the card's own release, else
the link on the original edition. It never matches by title or name.

| | |
| --- | --- |
| Source | `editions`, also-on source |
| Runs on | Track links with exactly one `mb:recording:…` key; album links with an `mb:release:…` or `mb:release-group:…` key |
| Asks | MusicBrainz `ws/2`: the releases of the recording or release group (`GET /release?recording=…` or `?release-group=…`, with their URL relations); for tracks, each track's album on its own service (iTunes, Spotify, TIDAL, Deezer) |
| Credentials | None of its own. Spotify and TIDAL tracks are placed with the credentials of the `spotify` and `tidal` sources |
| Rate | 1 request per second on `musicbrainz.org`; each service at its own pace |
| Rows live | 30 days; rows expire after 60 |
| Keys | None of its own. The derived links it adds are keyed by their release (`mb:release:…`) |
| Links | On an album card: the card's own release's album pages on other services, exact |
| Facts | `release`, `original_release` |
| Switch | `[also_on.sources.editions] enabled`, on by default |

`editions` runs last, after every source that finds links, so it can place what they found.

## What it does

### The candidates

The links it places are the ones the card may show: the other members of the card's
[group](../api.md#groups) (tagged members first), then the derived links that other sources
recorded for the group (those most members name first), at most 256. Of these it keeps the links
at the card's level on another service than the card's own. It never reads an album page nobody
linked.

### The original release group and edition

1. **The releases.** For a track, the source lists the recording's official album releases, up to
   four pages of 100. When none of them is an album with no secondary type, it also lists the
   recording's singles and EPs (one page). For an album, it lists the release group's official
   releases, up to two pages of 100. The release group is the one the card's key names, else the
   card's own release's.
2. **The original release group.** For a track, this is the first release group in this order:
   - no secondary type (a compilation, a soundtrack, a live album) before one with;
   - then an album, a single, an EP, then any other type;
   - then the earliest first release.

   A song first released on a soundtrack or a live album has only groups with secondary types, so
   the earliest of those is its original. For an album, the original group is its own.
3. **The original edition.** These are the original group's official releases that name no later
   edition, the earliest first. Of one date, digital releases come first, then worldwide ones. A
   release names a later edition when its disambiguation, or its title past the group's title
   (`… (Deluxe Edition)`), says:
   - deluxe, legacy, anniversary, remaster(ed), expanded, reissue(d), collector's, bonus, box
     set, special edition, super deluxe;
   - hi-res: `hi-res`, `high resolution`, a bit depth over 16 (`24-bit`) or a sample rate over
     48 kHz (`96 kHz`).

   "Edition" alone (standard, original, explicit) names none, and neither does a CD's
   `16-bit / 44.1 kHz`. A store's own copy of the original (`Apple Digital Master`,
   `Mastered for iTunes`) is no later edition either.

### The card's own release

- For a track: the releases MusicBrainz relates to the album page of the album the track's own
  service names (at most four). If the card's own album can't be read, every link keeps its
  place (see below), because the card's own release is the rank all others count against.
- For an album: the releases its `mb:release:…` keys name, else the releases MusicBrainz relates
  its URL to (at most four).

### Ranks

The album pages MusicBrainz relates to the card's own release rank 0. Those of the original
edition's releases rank 1, 2, … in the order above. So a service's link lands on its earliest
original release that has a page there: a 1975 release with a Spotify page before a 2011 one, and
a later digital release only when no earlier one has a page on that service.

The pages come with the list of releases (their URL relations), so no release is read on its own,
except a card's own release the list leaves out.

### Placing a link

A track link is on an album when its own service names that album as the track's:

| Service | How it's asked | When |
| --- | --- | --- |
| Apple Music | iTunes `GET https://itunes.apple.com/lookup?id={track}&entity=song` | While `itunes` is on |
| Spotify | Spotify Web API (`/v1/tracks/{id}`), else the track's public page (`music:album`) | While `spotify` is on, with its credentials or with `[also_on] spotify_pages` |
| TIDAL | TIDAL API (`/v2/tracks/{id}?include=albums`): every album TIDAL names for the track | While `tidal` is on and has its credentials |
| Deezer | Deezer API (`GET https://api.deezer.com/track/{id}`) | While `deezer` is on |

The card's own Spotify track, a link someone referenced, is read on its public page where the API
doesn't answer, whatever `spotify_pages` says. `[also_on] spotify_pages` is on by default. Links
on other services (Qobuz, YouTube, …) name no album here.

An album link is on the release whose album pages name it.

### Each link's outcome

| Outcome | When | Rank |
| --- | --- | --- |
| Placed | On the card's own release, or on an original-edition release | 0, or 1, 2, … |
| On another release | Its service names another album, and an own or original release has a page on that service (a later edition, a compilation). A Discogs master is the release group itself, never another release | 1000 |
| Gone | Its service names no album for it (a track it doesn't list), so it never wins over one that does | 1000 |
| Unplaced | No own or original release has a page on its service. Among unplaced links, the card's order is unchanged | None (no row) |
| Kept | The run can't tell (below). The place recorded before stays | As before |

A link keeps its earlier place when:

- its service failed (the run comes back after five minutes);
- its service refused the operator's credentials (as Spotify does for an app whose owner has no
  premium plan);
- the release list was cut short (MusicBrainz names more releases than the pages read): then only
  the card's own release ranks, and links not on it keep their places;
- more than eight track links of one service wait, in the candidates' order;
- the card's own album failed: then every link keeps its place.

A kept place lasts until it expires, twice the source's lifetime after it was recorded. Only
MusicBrainz failing (the release list, the card's own album's URL lookup) fails the whole run.

## Keys, links and facts

### An album card's own release's pages

On an album card, the album pages MusicBrainz relates to the card's own release on other services
(its album on Deezer, TIDAL, Apple Music, …) are derived links of this source. Each is keyed by
its release (`mb:release:{mbid}`) and placed first (rank 0).

`musicbrainz` derives none for an album link it relates to several releases of
one group: it keys the group, which names no pages. With this source, an album another source
lists for the same work (Wikidata may list several Deezer albums) never wins over the card's own
release's; it is on another release.

### Facts

| Fact | Meaning |
| --- | --- |
| `release` | The MBID of the card's own release (at most four) |
| `original_release` | The MBID of an original-edition release that has a page on a service (at most four) |

The places are stored per card and live as the source's other rows do.

### When a group changes

When a member joins or leaves a group, or a link becomes another's alias, the sources of the
group's music cards that carry a MusicBrainz key run again in ten minutes. Each card then places
the links it may show after the change.

## Cost

Per card:

- up to four pages of the recording's releases, or up to two of an album's release group;
- for a track: the card's own track's album (one request to its service, and one MusicBrainz URL
  lookup per album), and each candidate track's album, at most eight per service;
- a card's own release the list leaves out: one read.

MusicBrainz answers are cached for `limits.discovery_cache_secs` (an hour by default). A track
card with no link a service can place reads nothing. An album card still reads its own release's
pages.

To place the links of every stored card, run `pinakesd reextract also-on`, which queues every
resource's also-on run ([operations](../operations.md#also-on)).

## Turn it off

```toml
[also_on.sources.editions]
enabled = false
```

Its stored places, links and facts stop showing at once. Cards then pick each service's link as
they pick among unplaced links ([also on](../api.md#also-on)), without preferring the card's own
release or the original edition.
