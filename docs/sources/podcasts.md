# `podcasts`: Apple Podcasts, Podcast Index and Wikidata for a show

An [also on](../api.md#also-on) source.

`podcasts`: an Apple Podcasts link (`podcasts.apple.com/{cc}/podcast/{slug}/id{n}`
in any spelling, an episode with `?i={n}`), a player link that opens a show by its Apple ID
(`pca.st/itunes/{n}`, `castro.fm/itunes/{n}`, `overcast.fm/itunes{n}` with or without its slug), a
Podcast Index page
(`podcastindex.org/podcast/{feed id}`, an episode with `?episode={id}`) or a podcast feed's card
(a `PodcastSeries`, or a `PodcastEpisode` with its item's GUID) is looked up exactly:
- iTunes must know an Apple ID as a show. The show's key is `podcast:apple:{id}` (a player link
  has it from its URL alone), and it links Apple Podcasts, Pocket Casts, Castro and Overcast.
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
  that page, and the page is that item's own (no other item links it, and it is not the show's own link or a site's front page). Such a page is typed `PodcastEpisode` from its next refresh. A feed item finds its Apple track among the show's
  latest 200 episodes by its GUID.
- **What an episode's item key adds.** Once an episode has its item key, each of the following
  is keyed by it, so a tagged copy of any of them joins the episode:
  - The item's own page (its `<link>`: the show's site, or a YouTube video when the show
    publishes there), when no other item of the feed links it and it is not the show's own link or a site's front page. It is read in the vouched feed at Podcast Index's URL.
  - The RSS item, `{feed}?guid={item GUID}`, which a feed card reads as that episode.
  - Podcast Index's page of it (`episodes/byguid` in that feed). An episode's own Podcast
    Index page is the card.
  - Each file it plays: `podcast:enclosure:{SHA-256 of the enclosure URL}`, without its
    fragment. The source takes these from Podcast Index's record and iTunes': the feed's own
    item names its file too, but any feed may name any file, so that is a key only when one of
    the two services names the same file. One episode may play several files: iTunes may read
    another copy of a show's feed, which names the file differently. `GET /v1/search?key=podcast:enclosure:{URL}` takes the file's
    URL too.
- **An Apple episode's file.** The file iTunes' record of an Apple episode names is a key
  whether or not the show's GUID is vouched, just as its track is: Apple is the authority for
  its own episodes. iTunes may know an item by another GUID than the show's feed does (Darknet
  Diaries' "Conti"). Then the file is what joins Apple's episode to the show's own page.
- **A player's show page.** A page that names a show's feed without being the feed's own card
  (a Fountain show page, the `fountain` source) gets the show's keys only when the feed names it
  as the show's page, its channel `<link>`; otherwise it lists the show's links, none of them a
  key of the page.
- **A player's episode page.** A Castro episode page (the `castro` source) plays a file and
  names its show's Apple ID. The episode is the one among the show's latest 200 that iTunes
  lists with that very file. It gets that episode's keys, and the item key when the show is
  vouched. Castro's word alone is no key. Pocket Casts' and Overcast's episode pages are never
  read: Pocket Casts refuses Pinakes, and Overcast asks a reader to sign in.
- **What joins no episode yet.** These episode links name no item GUID or file Pinakes can
  read, so they join only when they are an episode's own `<link>`:
  - a Spotify episode;
  - a Pocket Casts or Overcast episode page;
  - a Podchaser page;
  - a YouTube video.

  Title and date within one show would only ever be a hint (`maybe_same`), never a key. That
  hint is not built: a Spotify episode's show and date need Spotify's episode record, and the
  Spotify source answers only once the app owner has Premium.
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
  then Podcast Index's record of the episode (its episode page's, or the `episodes/byguid` answer
  that finds that page), then iTunes' (`episodeUrl`), and records the enclosure as the facts
  `stream_url`, `stream_type` and `stream_duration`.
  - A card without a [stream](../api.md#stream) of its own takes it at its next read. The
    source asks for that read at once.
  - A link that isn't a public stream, or is longer than a fact's 512 bytes, is none.
  - A Fountain episode's feed is read for the stream even when no GUID is vouched, unless the
    card plays a stream of its own. A stream it took from this source is no reason to skip the
    read: each lookup hears of it again, or the card would lose it at its next refresh.
  - A stream joins nothing: a copy of a show's feed plays its own audio, and gets no key from
    it.

## Operations

Each podcast show costs one iTunes lookup, two or three Podcast Index requests (the last asks
who owns the show's GUID), its feed (usually still cached from its card) and two Wikidata
requests; each episode up to four Podcast Index requests (the last finds its page there), two
iTunes lookups and, once its show is vouched, its feed (cached for an hour); each page whose head names an RSS feed that feed (cached for an hour,
read up to 8 MiB), and Podcast Index only when one item of it with audio or video is that page.

Podcast Index is asked only with a key and secret: `[podcast_index] key` and `secret`, or
`PINAKES__PODCAST_INDEX__KEY` and `PINAKES__PODCAST_INDEX__SECRET` (free from
api.podcastindex.org). Without them a show joins through its Apple ID only: no GUID is a key, a
feed gets no Apple ID, a Podcast Index page has nothing to read, and episodes join only by their
Apple track. The key and secret never appear in logs, the operator info or errors. A refused key
fails no lookup and brings no run back early: what the other services answer is added, and nothing the source recorded before is deleted (a row no service gives any more leaves when it expires). An outage of a secondary service keeps what the other services answered,
and the run comes back after a failed run's wait; when nothing at all was found the lookup fails and the stored rows stay. Such failures of a secondary service
(Podcast Index, iTunes, Wikidata, the feed) are counted in
`pinakes_also_on_service_errors_total{service,class}`; a refused key (a 401, or a 403 whose
`description` names the key) is also counted in `pinakes_source_token_refusals_total{source="podcast_index"}`, which the `ProviderKeyRefused`
alert reads.
