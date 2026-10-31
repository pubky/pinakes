# Castro

Episodes on Castro. Castro has no public API, so this source is a page hook: the web route
reads the page as usual, and the page's own metadata names the card (its `og:title`, "Darknet
Diaries: Conti (59m)", its description and cover). The hook adds what makes it the episode: the
file it plays and its show. No key, no account.

## Pages

`https://castro.fm/episode/{id}`. The page names:
- the file its player plays: the one `<source>` of `<audio id="co-supertop-castro-episode">`,
  the show's own enclosure;
- the show's Apple Podcasts ID: the one ID its "Listen on" links name (Apple Podcasts'
  `…/id{n}`, `pca.st/itunes/{n}`, `castro.fm/itunes/{n}`).

A page with no player, more than one file, or a file that is no public link (signed, expiring)
keeps its page card. A page naming two Apple IDs names none. Nothing behind a login is read.

## What it reads

Nothing beyond the page the web route fetched.

## Card

The page's card, typed `PodcastEpisode`, with the file as its [`stream`](../api.md#stream) and
the show's Apple ID as `podcast_apple`. The also-on `podcasts` source then finds
the episode on Apple Podcasts. It takes the one episode among the show's latest that iTunes
lists with that very file, and from there the episode's keys. Castro's word alone is no key: a
page joins by what Apple confirms. `details.from` is `castro`.

| Fact | Meaning |
| --- | --- |
| `podcast_apple` | The show's Apple Podcasts ID |

## Examples

- `https://castro.fm/episode/A1tnxb` (Darknet Diaries, "Conti")
