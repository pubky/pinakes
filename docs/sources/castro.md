# Castro

Pinakes improves the cards of episode pages on Castro. Castro has no public API, so this source is a
page hook: it claims no links, the web route reads the page as usual, and the hook adds the file the
episode plays and its show's Apple ID from that page. No key is needed.

| | |
| --- | --- |
| Source | `castro`, official source, page hook |
| Links | `castro.fm/episode/{id}`, read by the web route |
| Fetches | Nothing beyond the page the web route fetches |
| Credentials | None |
| Rate | Shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | `PodcastEpisode` |
| Keys | None (facts only); the also-on source `podcasts` keys its cards |
| Credit | None |
| Switch | `[sources.castro] enabled`, on by default |

## Links

The hook runs on `https://castro.fm/episode/{id}`, with or without `www.` and with any parameters,
and on a link that redirects to one. An ID is letters and digits.

Not read here: show pages (`castro.fm/podcast/{id}`) and Apple ID links (`castro.fm/itunes/{id}`).
The also-on source `podcasts` looks up the Apple ID links.

## What it fetches

Nothing of its own. From the page's HTML the hook reads:

- **The file**: the one `<source>` of `<audio id="co-supertop-castro-episode">`, which is the show's
  own enclosure.
- **The show's Apple Podcasts ID**: the one ID the page's links name, among every `href` of the form
  Apple Podcasts' `…/id{n}`, `pca.st/itunes/{n}` or `castro.fm/itunes/{n}`. A page that names two
  IDs names none.

A page without a player, with more than one file, or whose file is no public link (a signed or
expiring one) keeps its page card.

## Card

The card is the page's, from its own metadata (`og:title`, description and cover), with these
changes:

- the type is `PodcastEpisode`;
- the file is the [stream](../api.md#stream);
- `from` is `castro`.

| Fact | Meaning |
| --- | --- |
| `podcast_apple` | The show's Apple Podcasts ID |

## Keys and groups

The page records no key: Castro's word alone joins nothing. The also-on source `podcasts`
takes the one episode among the show's latest 200 that iTunes lists with this very file, and gives
the page that episode's keys.

## Turn it off

```toml
[sources.castro]
enabled = false
```

Castro episode pages then keep the card their own metadata makes. The link alone still types it
`PodcastEpisode`, but it has no stream and no keys.

## Examples

- `https://castro.fm/episode/A1tnxb` (Darknet Diaries, "Conti")
