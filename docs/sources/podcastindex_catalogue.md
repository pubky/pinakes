# `podcastindex` catalogue: every live podcast

This catalogue imports Podcast Index's weekly database: one
[catalogue card](../operations.md#catalogues) for each podcast feed that isn't dead (about 4.8
million). The cards are searchable whether or not anyone shared them. A shared feed, Apple
Podcasts, Podcast Index or site link of a podcast joins the card's group.

| | |
| --- | --- |
| Catalogue | `podcastindex` |
| Reads | `https://public.podcastindex.org/podcastindex_feeds.db.tgz` (an SQLite database in a gzipped tar, 1.9 GB) |
| Credentials | None |
| Rate | The archive's version (`HEAD`) once a day; the archive once a week, when it is new |
| Refresh | Weekly, as Podcast Index publishes it |
| Needs | `http.contact`; 12 GiB free under `catalogues.dir/podcastindex` for the unpacked database (its cap; 5 GB today; a resumed run counts what its kept database holds; a local `.db` file needs none) |
| Card type | `PodcastSeries` |
| Keys | `podcast:guid:…`, `podcast:apple:…`, `url:…` |
| Credit | `from: "podcastindex"`; the card's link is the feed |
| Switch | `[catalogues.podcastindex] enabled`, on by default |

## What it reads

A run downloads the archive and unpacks its `podcastindex_feeds.db` into
`catalogues.dir/podcastindex` (an archive that ends inside the file is refused, `archive`), reads
the `podcasts` table's feeds with `dead = 0` in ID order, and deletes the file when it is done.
A table without the columns read, a virtual table, a generated column, or a view or trigger in
the file is refused (`schema`). The file is opened read only, with SQLite in defensive mode,
its schema untrusted, and no value over 64 MiB; texts are read cut (links 2,048 bytes, the
description 4,096, the rest 512). The archive may hold 8 GiB and the database 12 GiB; each of
the read's two passes over the table at most 50 million rows (`size`), and the reading at most
six hours. Podcast Index's server does not answer ranged requests, so a broken download starts
again; each attempt writes a part file of its own, and a run that is cut off stops its download
within a megabyte. A full import that finds no live feed is refused (`empty`) and removes nothing.
Podcast Index publishes no checksum of the archive: its integrity rests on TLS, the archive's
length and the gzip stream's own CRC.

Private feeds are never imported: a feed on a host that serves paid subscribers' feeds
(`patreon.com`, `supercast.com`, `supercast.tech`, `supportingcast.fm`, `glow.fm`,
`memberful.com`, `steadyhq.com`, and their subdomains), or whose URL's query carries a credential
(`token`, `key`, `auth`, `access`, `secret`, `sig`, `signature`, `password`, a name ending in one
such as `access_token` or `api_key`, or a `uid` with a `hash`), makes no card; so does a capability
link. A subscriber's token in a public index still opens their subscription. Every other live feed
becomes a card. The database has no column for a feed's `<podcast:block>`, so that is not read.

## Cards

| Field | Holds |
| --- | --- |
| `uri` | The feed's URL |
| `name`, `description` | Its title and description |
| `labels` | Its categories (up to ten), as tag labels |
| `fame` | Its Podcast Index popularity, 0-9, divided by 9 |
| `details` | `author`, `language`, `website`, `explicit` (the episode count and the newest episode change every week and are read from the feed when the card is opened) |

## Keys and groups

| Key | From |
| --- | --- |
| `podcast:guid:<guid>` | Its `podcast:guid`, in lower case |
| `podcast:apple:<id>` | Its Apple Podcasts ID |
| `url:` of `https://podcastindex.org/podcast/<feed id>` | Its Podcast Index page |
| `url:` of its original feed URL | When it differs from the feed |
| `url:` of its site | When exactly one feed names that site (a hosting platform's page that many feeds name is no one podcast's) |

A Podcast Index card backs no link: the feed is fetched for its episodes. The feed, a show's
Apple Podcasts page and the links its `url:` keys name join the card's group instead
([shared links](../operations.md#shared-links-a-catalogue-holds)).
When a card is written, the links already stored that it joins are marked at once by their
exact spelling: the feed, its original URL, its site, and the show's Apple Podcasts page as
`https://podcasts.apple.com/podcast/id<id>` or `/us/podcast/id<id>`. An Apple Podcasts page stored
in another spelling (another country, or with the show's name, as most shared ones are) joins at
its next refresh, or the next time it is shared, not when the card is written.

## Updates

Each day the importer asks for the archive's version (its ETag). When it is new, the whole
database is read again: changed cards are written, unchanged ones are not touched, and feeds no
longer listed are removed. An update that would remove more than a tenth of the cards (and more
than ten) is refused, `refused`: nothing is removed, the unpacked database is deleted, and the
failure is recorded once (`catalogue status` shows `refused: <version>`, and
`pinakes_catalogue_refused{catalogue="podcastindex"}` is 1). So is an archive that can't be read
as it is (`archive`, `size`: broken or over its cap; `database`, `schema`: no Podcast Index
database in it), and a full import that reads no live feed (`empty`). The daily runs then end at
once without downloading that archive again until a newer one appears, until a full import is
asked for (`pinakesd catalogue import podcastindex --request` with the daemon running, or
`catalogue import podcastindex` with it stopped), or until `pinakesd catalogue clear-refusal
podcastindex` clears it ([a refused dump or file](../operations.md#a-refused-dump-or-file)). The allowed lag is 9 days.

## Sample run

`scripts/catalogue-sample podcastindex` reads the first 300 MB of the real archive, copies the
feeds that part holds into a whole database (`scripts/catalogue/sqlite-head.py`) and imports it
into a throwaway database ([sample runs](../operations.md#catalogue-sample-runs)).

## Turn it off

`[catalogues.podcastindex] enabled = false`: the cards leave search at once.
