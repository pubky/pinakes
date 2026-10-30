# `podcasts`: Apple Podcasts, Podcast Index and Wikidata for a show

An [also on](../api.md#also-on) source.

`podcasts`: an Apple Podcasts link (`podcasts.apple.com/{cc}/podcast/{slug}/id{n}`
in any spelling, an episode with `?i={n}`), a player link that opens a show by its Apple ID
(`pca.st/itunes/{n}`, `castro.fm/itunes/{n}`), a Podcast Index page
(`podcastindex.org/podcast/{feed id}`, an episode with `?episode={id}`) or a podcast feed's card
(a `PodcastSeries`, or a `PodcastEpisode` with its item's GUID) is looked up exactly:
- iTunes must know an Apple ID as a show. The show's key is `podcast:apple:{id}` (a player link
  has it from its URL alone), and it links Apple Podcasts, Pocket Casts and Castro.
- Apple is the authority for its own IDs. An Apple ID that Podcast Index names for a feed counts
  only when iTunes' own record of that show names this feed (`feedUrl`, compared without scheme
  and trailing slash); and an Apple show link takes Podcast Index's feed for its ID only when
  that feed is iTunes' feed. Otherwise there is no Apple key, Apple link or Apple episode for
  the feed, and no Podcast Index feed for the show. (The Daily is such a show: iTunes and
  Podcast Index read different feeds of it.)
- A show's GUID is a key, `podcast:guid:{uuid}`, only when Podcast Index vouches for it: its
  answer for this feed URL, Apple ID or feed ID names that GUID (an Apple ID it names counts only
  once iTunes knows it), and its feed for that GUID (`byguid`) is this same feed. A copy of a
  show's feed carries the show's GUID too, but Podcast Index names the show's own feed for it.
  This needs an operator's Podcast Index key ([operations](#operations)).
- The show's `podcast_guid` fact is the GUID its feed names (read at the card's own feed URL,
  never through a redirect), else Podcast Index's, else the one derived from the feed's URL as
  the podcast namespace says. A GUID nobody vouches for is never a key: a copied feed can name
  any show's GUID, and a derived one changes with the feed's URL.
- An episode's keys are its Apple track, `podcast:apple_episode:{id}`, and
  `podcast:item:{show GUID}:{SHA-256 of the item GUID}` from its show's vouched GUID. An Apple
  episode takes the item GUID from iTunes, a Podcast Index episode from Podcast Index, and a
  page only when the feed (its feed card's, or the RSS feed its own head names, followed through
  redirects, read up to 8 MiB) is vouched and has one item with audio or video whose `<link>` is
  that page. Such a page is typed `PodcastEpisode` from its next refresh. A feed item finds its Apple track among the show's
  latest 200 episodes by its GUID.
- A Podcast Index page links the show's feed and its Apple, Pocket Casts and Castro pages.
- The show's Wikidata item (by its Apple ID) gives its Q-id as a key and its Spotify, Podcast
  Index, Podchaser, Pocket Casts and Castbox pages.
- Each link carries the key it was found through: the Apple, player and Wikidata item pages the
  Apple ID, a Podcast Index page or feed the GUID Podcast Index vouches for (else the Apple ID),
  an episode's Apple page its track. A tagged Spotify show link therefore joins its show.
- A link never lists its own service. A Podcast Index failure (a refused key, an outage) never
  fails the lookup: what the other services answered still counts.
- A Pocket Casts short link (`pca.st/{slug}`) carries no Apple ID and is not looked up.
- **Stream.** An episode's lookup hears of its enclosure. The source prefers its feed's item,
  then Podcast Index's record of the episode, then iTunes' (`episodeUrl`), and records the
  enclosure as the facts `stream_url`, `stream_type` and `stream_duration`.
  - A card without a [stream](../api.md#stream) of its own takes it at its next read. The
    source asks for that read at once.
  - A link that isn't a public stream, or is longer than a fact's 512 bytes, is none.
  - A Fountain episode's feed is read for the stream even when no GUID is vouched.
  - A stream joins nothing: a copy of a show's feed plays its own audio, and gets no key from
    it.

## Operations

Each podcast show costs one iTunes lookup, two or three Podcast Index requests (the last asks
who owns the show's GUID), its feed (usually still cached from its card) and two Wikidata
requests; each episode up to three Podcast Index requests, two iTunes lookups and, for an
episode's page, its feed; each page whose head names an RSS feed that feed (cached for an hour,
read up to 8 MiB), and Podcast Index only when one item of it with audio or video is that page.

Podcast Index is asked only with a key and secret: `[podcast_index] key` and `secret`, or
`PINAKES__PODCAST_INDEX__KEY` and `PINAKES__PODCAST_INDEX__SECRET` (free from
api.podcastindex.org). Without them a show joins through its Apple ID only: no GUID is a key, a
feed gets no Apple ID, a Podcast Index page has nothing to read, and episodes join only by their
Apple track. The key and secret never appear in logs, the operator info or errors. A refused key
or a Podcast Index outage fails no lookup: the source keeps what the other services answered,
and asks again later only when nothing at all was found. Such failures of a secondary service
(Podcast Index, iTunes, Wikidata, the feed) are counted in
`pinakes_also_on_service_errors_total{service,class}`.
