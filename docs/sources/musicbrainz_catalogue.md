# `musicbrainz` catalogue: every artist, album and song of MusicBrainz

This catalogue imports MusicBrainz's JSON dumps: one [catalogue card](../operations.md#catalogues)
for each artist, release group (album) and recording (song). The cards are searchable whether or
not anyone shared them, and a shared musicbrainz.org page, ISRC page or streaming link that
MusicBrainz relates to an item gets its card from the catalogue without a fetch.

| | |
| --- | --- |
| Catalogue | `musicbrainz` |
| Reads | `https://data.metabrainz.org/pub/musicbrainz/data/json-dumps/`: `LATEST`, then the newest dump's `release.tar.xz`, `release-group.tar.xz`, `artist.tar.xz`, `recording.tar.xz` and `SHA256SUMS` |
| Credentials | None |
| Rate | One file at a time, streamed; nothing is kept but sorted runs under `catalogues.dir/musicbrainz` |
| Refresh | A new dump is looked for every 12 hours; MusicBrainz publishes two a week |
| Card types | `Person` or `MusicGroup` (artists), `MusicAlbum` (release groups), `MusicRecording` (recordings) |
| Keys | `mb:artist:…`, `mb:release-group:…`, `mb:recording:…`, `isrc:…`, `wikidata:Q…`, `url:…` |
| Credit | `from: "musicbrainz"`; the card's link is its musicbrainz.org page |
| Switch | `[catalogues.musicbrainz] enabled`, on by default; `limit` for a small instance |

## What it reads

Each file is a `tar.xz` archive holding one JSON line per entity (`mbdump/<entity>`), as
MusicBrainz's web service writes it, and the dump's `TIMESTAMP`. The importer streams them in this
order and keeps only what a card needs:

1. `release`: each release's artists, release group and recordings (with the release's date and
   its release group's title, date and genres);
2. `release-group`, `artist`: one entity each;
3. `recording`: the recordings on no release.

What belongs to one item (a recording on 200 releases) comes together in sorted runs under
`catalogues.dir/musicbrainz`, so the dump is never held in memory. A file whose SHA-256 is not the
one in the dump's `SHA256SUMS` stops the run before any card is written (`checksum`). A line longer
than 128 MiB is skipped and counted; more than 100 lines that don't parse, and more than one in a
thousand, stop the run (`parse`).

## Cards

| Field | Artist | Release group | Recording |
| --- | --- | --- | --- |
| `uri` | `https://musicbrainz.org/artist/<mbid>` | `https://musicbrainz.org/release-group/<mbid>` | `https://musicbrainz.org/recording/<mbid>` |
| `type` | `Person` for a person, else `MusicGroup` | `MusicAlbum` | `MusicRecording` |
| `name` | Its name | Its title | Its title |
| `other_names` | Its aliases | Its aliases | Its aliases and the titles its tracks carry |
| `description` | Its type and area ("Group from United States"), its disambiguation | Its type and artist credit ("Album by Rosemary Clooney"), its year, its disambiguation | Its artist credit, its earliest release's album and year, its length, its disambiguation |
| `labels` | Its genres | Its genres | Its genres, else its earliest release's release group's |
| `related_uris` | Its `official homepage` relations | | |
| `fame` | From the releases crediting it | From its releases | From the releases it is on |
| `details` | `wikipedia` | `artists`, `wikipedia` | `artists`, `release_group` |

- Labels are MusicBrainz's genres by votes, at most 10, as tag labels
  ([labels](../operations.md#what-a-card-holds)).
- Fame is `ln(1 + releases) / ln(1001)`, at most 1: one release gives 0.10, ten 0.35, a hundred
  0.67, a thousand or more 1.
- `details.wikipedia` is the English Wikipedia article a `wikipedia` relation names, else the
  first; a link built from the card takes its summary
  ([shared links](../operations.md#shared-links-a-catalogue-holds)). `artists` names up to four
  artist MBIDs of the credit, `release_group` the release group of the earliest release.

## Keys and groups

| Key | When |
| --- | --- |
| `mb:<entity>:<mbid>` | Always |
| `isrc:<ISRC>` | A recording's ISRCs, at most eight, each only when no other recording of the dump carries it |
| `wikidata:Q…` | An artist's or release group's `wikidata` relation, when it names exactly one item |
| `url:<resource id>` | Each current URL relation that is a music service's page at the item's level: a recording's tracks and videos (Spotify, Apple Music, Deezer, TIDAL, YouTube, …), an artist's artist pages (also Bandcamp and Discogs artists), a release group's albums and Discogs master, and its releases' album pages (not their Discogs releases, which are editions) |

The keys never cross levels: a recording's album and artists are in its `details`, never in its
keys, so a song and its album stay apart. A shared link joins a card:

- by its link: a musicbrainz.org artist, release group or recording page;
- by a key it names: an ISRC page (`musicbrainz.org/isrc/<ISRC>`) whose ISRC one recording
  carries;
- by its `url:` key: a streaming or shop page MusicBrainz relates to the item.

A card whose `wikidata:` key a Wikidata card carries is one search row with it.

## Updates

A full import runs first. After it, the importer looks for a new dump every 12 hours. A new dump is
an update: it reads the four files again, sorts the stored cards' content hashes with them, and
then writes only the cards that are new or changed and removes the cards the dump no longer has;
an unchanged card is not written. The text index takes the update's changes through its queue. The
cards are as current as the dump's `TIMESTAMP` (`current_as_of`); MusicBrainz publishes twice a
week, so `CatalogueUpdateLag` allows five days.

An update that would remove more than a tenth of the cards stops before it removes any (`removals`,
in `catalogue status` and `CatalogueImportFailed`). If the dump is right, run a full import of it:
`pinakesd catalogue import musicbrainz`, which removes the cards it does not read.

A run cut off by a restart or a failure resumes after the last item it wrote, from the sorted runs
when they are complete, else it reads the dump again.

## A small instance

`[catalogues.musicbrainz] limit = N` imports at most N cards: the first releases of the dump whose
recordings, release groups and credited artists fit in N, then only those release groups and
artists. Recordings on no release are not read. The read stops once the releases are full, so a
small limit reads a few megabytes of the release file, but the release group and artist files are
read until every one wanted is found.

## Run it by hand

```sh
pinakesd --config <path> catalogue import musicbrainz [--from DIR|URL] [--limit N] [--update]
```

It runs one full import (or with `--update` one update) now, through the daemon's code, with the
catalogue's lock: stop the daemon first, or switch the catalogue off there. `--from` reads a local
directory holding one dump's files (and its `SHA256SUMS`, when it is to be checked) or another
mirror's `json-dumps/` URL; `--limit` takes the place of the configured limit. A full import
started by hand takes up one that was cut off, else begins a new one.

`scripts/catalogue-sample musicbrainz` imports a sample of the two newest dumps (the first
megabytes of each file) into a throwaway database and checks it end to end: the cards, a search,
shared links joining their cards through `POST /v1/resolve`, an update that rewrites only changed
cards, and the catalogue switched off.

## Turn it off

```toml
[catalogues.musicbrainz]
enabled = false
```

Its cards leave search and lookups at once, and the links built from them are fetched again
([turn a catalogue off](../operations.md#turn-a-catalogue-off)). The rows stay until
`pinakesd catalogue drop musicbrainz`.
