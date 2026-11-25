# `podcasts`: Apple Podcasts, Podcast Index and Wikidata for a show

This source runs on links to podcast shows and episodes. It finds the show or episode on Apple
Podcasts, Podcast Index and other services by exact IDs, never by title. It records keys and
[also on](../api.md#also-on) links, and the episode's audio. It works without credentials. With an
operator's Podcast Index key it also keys shows by GUID and episodes by feed item.

| | |
| --- | --- |
| Source | `podcasts`, also-on source |
| Runs on | Apple Podcasts, Pocket Casts, Castro, Overcast and Podcast Index links, podcast feeds' cards, Fountain and Castro pages, and web pages whose head names a feed ([links](#links-it-reads)) |
| Asks | iTunes (`GET https://itunes.apple.com/lookup?id=…`), Podcast Index (`https://api.podcastindex.org/api/1.0/podcasts/…` and `/episodes/…`), the show's feed, Wikidata (`https://www.wikidata.org/w/api.php`) |
| Credentials | Optional: `[podcast_index] key` and `secret`, or `PINAKES__PODCAST_INDEX__KEY` and `PINAKES__PODCAST_INDEX__SECRET` ([what they add](#podcast-index-credentials)) |
| Rate | At most about 20 requests a minute on `itunes.apple.com` and 200 a minute on `www.wikidata.org` (the services' published limits), shared with every source that asks them; the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower. `api.podcastindex.org` and feeds: the shared default |
| Rows live | 30 days; rows expire after 60 |
| Keys | `podcast:apple:…`, `podcast:guid:…`, `podcast:apple_episode:…`, `podcast:item:…`, `podcast:enclosure:…`, `wikidata:Q…` |
| Links | The show's and episode's pages on the services above, its feed and the page the feed names, exact |
| Facts | `podcast_guid`, `stream_url`, `stream_type`, `stream_duration` |
| Switch | `[also_on.sources.podcasts] enabled`, on by default |

## What it does

### Links it reads

| Link | What it names |
| --- | --- |
| `podcasts.apple.com/{cc}/podcast/{slug}/id{n}`, with or without the country and the slug, and the same on `itunes.apple.com` | A show by its Apple ID; with `?i={n}`, an episode by its Apple track ID |
| `pca.st/itunes/{n}` (Pocket Casts), `castro.fm/itunes/{n}` (Castro), `overcast.fm/itunes{n}` with or without a slug (Overcast) | A show by its Apple ID |
| `podcastindex.org/podcast/{feed id}` | A show by its Podcast Index feed ID; with `?episode={id}`, an episode |
| A card with the fact `podcast_feed`: a podcast feed's own card, or a Fountain page | A `PodcastSeries` is the feed's show; a `PodcastEpisode` is the item with its `podcast_guid` |
| A `PodcastEpisode` card with a stream and the fact `podcast_apple`: a Castro episode page | The episode that plays that file, in the show with that Apple ID |
| Any other resolved card with the fact `feed` (the feed its page's head names) | Perhaps an item of that feed |

Apple Podcasts, Podcast Index and player links are typed by the link alone: `PodcastSeries` for a
show and `PodcastEpisode` for an episode. A page that gets an item key is typed `PodcastEpisode`
from its next refresh.

These are not looked up as episodes or shows:

- a Pocket Casts short link (`pca.st/{slug}`), which carries no Apple ID;
- episode pages on Pocket Casts and Overcast, Spotify episodes, Podchaser pages and YouTube videos.
  They join an episode only when they are its own page in the feed (its `<link>`). There is no
  match by title or date.

### A show by its Apple ID

1. iTunes must know the ID as a show. Otherwise an Apple Podcasts link gets nothing, and a player
   link gets only the key its URL carries (`podcast:apple:{id}`).
2. The show's key is `podcast:apple:{id}`. Its links are its pages on Apple Podcasts, Pocket Casts,
   Castro and Overcast.
3. Podcast Index's feed for the ID counts only when it is the feed iTunes names for the show
   (`feedUrl`).
4. The one Wikidata item with the Apple ID (P5842) gives its Q-id as a key and the show's Spotify
   (P5916), Podcast Index (P11740, when Podcast Index itself gave none), Podchaser (P7998) and
   Castbox (P9005) pages.

Apple is the authority for its own IDs. An Apple ID that Podcast Index names for a feed counts only
when iTunes' record of that show names the same feed. Two feed URLs are the same when only the
scheme, the host's case or trailing slashes differ. When iTunes and Podcast Index read different
feeds of a show, as they do for The Daily, the feed gets no Apple key, link or episode, and the
Apple show gets no Podcast Index feed.

### A show by its feed or its Podcast Index page

Podcast Index is asked for the feed (by URL or feed ID), and its answer must name what was asked. A
feed whose address carries a credential is never sent. When the Apple ID Podcast Index names is
confirmed by iTunes, the show gets everything an Apple show gets. A Podcast Index page also links
the show's feed.

A page that names a show's feed without being the feed's own card, such as a Fountain show page,
gets the show's keys only when the feed names it as the show's page (the channel's `<link>`).
Otherwise it lists the show's links, and none of them is a key of the page.

### The show's GUID

A show's GUID is a key only when Podcast Index vouches for it: Podcast Index's record of the feed
names the GUID, and its feed for that GUID (`byguid`) is this same feed. A copy of a show's feed
can carry the show's GUID, but Podcast Index names the show's own feed for it.

The `podcast_guid` fact of a show is the first of these:

1. the GUID its feed names (read at the card's own feed URL, never through a redirect);
2. Podcast Index's GUID;
3. for a feed's card, the GUID derived from the feed's URL, as the podcast namespace defines it.

A GUID that nobody vouches for is never a key. A copied feed can name any show's GUID, and a
derived one changes with the feed's URL.

### An episode

An episode is found in two steps.

**On Apple Podcasts.** Among the show's latest 200 episodes that iTunes lists, the source takes the
one with the track ID of the link, with the item's GUID, or with the file a player's page plays. A
GUID or a file that two episodes share gives none. The episode gets the keys
`podcast:apple_episode:{track}` and `podcast:enclosure:…` for the file iTunes names, and its Apple
Podcasts page. This works without a vouched GUID.

**As a feed item.** When the show's GUID is vouched and the item's GUID is known, the episode gets
the key `podcast:item:{show GUID}:{SHA-256 of the item GUID}`. The item GUID comes from iTunes for
an Apple or Castro episode, from Podcast Index for its own episode page, from the card's
`podcast_guid` for a feed card, and from the feed for any other page.

A page or a feed card gets the item key only when all of these hold:

- the feed is vouched. For a feed card it is the card's feed. For any other page it is the feed its
  own head names, followed through redirects;
- one item of the feed has the page as its `<link>`, and for a page known only by its head, that
  item has audio or video;
- the page is that item's own: no other item links it, and it is not the show's own link, a site's
  front page or a feed.

Once an episode has its item key, these are linked with it, so a tagged copy of any of them joins
the episode:

- the item's own page, which can be a YouTube video when the show publishes there;
- the RSS item, `{feed}?guid={item GUID}`, which a feed card reads as that episode;
- Podcast Index's page of the episode;
- each file that Podcast Index or iTunes says it plays, as a `podcast:enclosure:…` key of the card.

### A Castro episode page

A Castro episode page plays a file and names its show's Apple ID. The episode is the one
among the show's latest 200 that iTunes lists with that very file. Castro's word alone is no key.

### The episode's audio

The source records the enclosure it hears of as `stream_url`, `stream_type` and `stream_duration`.
It prefers the feed item's, then Podcast Index's, then iTunes'. A card without a
[stream](../api.md#stream) of its own takes it at its next read, which the source asks for at once.

- A link that is not a public stream, or is longer than 512 bytes, is not recorded.
- A Fountain episode's feed is read for the stream even when no GUID is vouched, unless the card
  plays a stream of its own.
- A stream joins nothing. A copy of a show's feed plays its own audio and gets no key from it.

## Keys, links and facts

| Key | Names | From |
| --- | --- | --- |
| `podcast:apple:{id}` | A show | iTunes; for a player link iTunes does not know, its URL |
| `podcast:guid:{uuid}` | A show | Podcast Index, when it vouches |
| `podcast:apple_episode:{track}` | An episode | iTunes |
| `podcast:item:{show GUID}:{SHA-256 of the item GUID}` | An episode | The vouched GUID and the item's GUID |
| `podcast:enclosure:{SHA-256 of the URL}` | A file an episode plays (an http or https URL, without its fragment) | iTunes or Podcast Index |
| `wikidata:{Q-id}` | A show | The item with its Apple ID |

A group holds at most one GUID and one Apple ID. An episode can have several enclosure keys, because
iTunes can read another copy of the feed than Podcast Index does. A file that only the feed names is
no key, because any feed can name any file. iTunes can also know an item by another GUID than the
show's feed does; then the file is what joins Apple's episode to the show's own page.

`GET /v1/search?key=podcast:{GUID}` finds a show. `key=` also takes every key as it is recorded, and
`key=podcast:enclosure:{URL}` takes the file's URL.

All links are exact. Each one carries the key it was found through, so a tagged Spotify show joins
its show:

| Link | Key |
| --- | --- |
| A show's Apple Podcasts, player and Wikidata item pages | `podcast:apple:…` |
| A show's Podcast Index page and feed | `podcast:guid:…`, else `podcast:apple:…` |
| An episode's Apple Podcasts page | `podcast:apple_episode:…` |
| An episode's own page, RSS item and Podcast Index page | `podcast:item:…` |

A card lists at most one link per service here, the first found, and never its own service.

| Fact | Meaning |
| --- | --- |
| `podcast_guid` | A show's GUID, vouched or not |
| `stream_url` | An episode's enclosure |
| `stream_type` | Its MIME type |
| `stream_duration` | Its length in seconds, when stated |

## Cost

| Card | iTunes | Podcast Index | Wikidata | Feed |
| --- | --- | --- | --- | --- |
| A show | Up to 1 | 2 | 2, when iTunes confirms the Apple ID | 1, for a feed's card |
| An episode | Up to 2 | Up to 3 | None | 1, when its show is vouched or its stream is wanted |
| A page whose head names a feed | Up to 2 | Up to 3, only when one item with audio or video is that page | None | 1 |

iTunes, Wikidata and feed answers are cached for `limits.discovery_cache_secs` (an hour by default).
Podcast Index answers are not cached. A feed larger than 8 MiB is not read. The iTunes lookups use
the storefront `[also_on] apple_storefront` (`us` by default).

### Podcast Index credentials

Podcast Index is asked only with both a key and a secret ([API keys](../api-keys.md)). They sign
each request and never appear in logs, errors or the operator info. Without them:

- no GUID is a key, so no episode gets an item key;
- a feed gets no Apple ID, and a Podcast Index page gets nothing;
- shows join by their Apple ID, and episodes by their Apple track and the file iTunes names.

### Failures

A failure of iTunes, Podcast Index, Wikidata or the feed never discards what the others answered.
It is counted in `pinakes_also_on_service_errors_total{service,class}`, and nothing the source
recorded before is deleted. A row that no service gives any more leaves when it expires.

- After an outage, the run comes back after a failed run's wait. When nothing at all was found, the
  lookup fails and is retried.
- After a refused key (a 401, or a 403 whose `description` names the key), the run does not come
  back early. The refusal is also counted in
  `pinakes_source_token_refusals_total{source="podcast_index"}`, which the `ProviderKeyRefused`
  alert reads.

## Turn it off

```toml
[also_on.sources.podcasts]
enabled = false
```

Its stored keys, links and facts stop showing at the next start, and its groups re-form without
them. Cards lose a stream and an episode type they took from it at their next read. The types that
come from the link alone stay. The `podcast_index` source still reads Podcast Index
pages.
