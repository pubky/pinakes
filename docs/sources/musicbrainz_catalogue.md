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
| Rate | `LATEST` once an hour; one file at a time, streamed; nothing is kept but sorted runs under `catalogues.dir/musicbrainz` |
| Refresh | A new dump is read when `LATEST` names one; MusicBrainz publishes two a week |
| Needs | `http.contact`: without it the importer does not start (every request names the operator's contact) |
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
`catalogues.dir/musicbrainz`, so the dump is never held in memory. The runs are merged down to at
most 256 before any card is written.

- **Checksums.** Each file read to its end is checked against the dump's `SHA256SUMS` before any
  card is written: a file whose SHA-256 differs, or one the list leaves out, stops the run
  (`checksum`; a file the mirror's list leaves out is not downloaded). A local dump without a
  `SHA256SUMS` is read unchecked, and so are the files a `limit` stops reading early.
- **Bounds.** A line longer than 128 MiB is skipped and counted. Each list of a line is read up to
  a cap (4,096 tracks a medium, 512 media, 2,048 relations, 256 aliases and genres, 64 ISRCs and
  credited artists) and the rest skipped. Of one item's records at most 8,192 holding 64 MiB make
  its card (every one counts for its fame). More than 100 lines that don't parse, and more than
  one in a thousand of the lines read, stop the run (`parse`).
- **Downloads.** A broken connection is asked for again from the byte where it broke (`Range`
  with `If-Range`, so a file replaced meanwhile is refused, never spliced), up to five times in a
  row (renewed after every 64 MiB read), and not at all when the mirror names no ETag or
  Last-Modified. A file may take 12 hours.

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
  ([labels](../operations.md#what-a-card-holds)). A recording without genres takes those of the
  release group of its earliest release that has some.
- Other names are the aliases in sort order, at most 16.
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

A full import runs first. After it, the importer reads `LATEST` once an hour; when it names a
newer dump than the cards are from, an update runs:

1. It reads the four files again and sorts the stored cards' content hashes with them.
2. A first pass lists the stored cards the dump no longer has. When they are more than a tenth of
   the cards, the update stops there with nothing written (`removals`, with the counts, in
   `catalogue status` and `CatalogueImportFailed`), and waits: it is not read again until `LATEST`
   names a newer dump. If the dump is right, ask for a full import of it, which removes every card
   it does not read: `pinakesd catalogue import musicbrainz --request` beside the running daemon
   (the update hands over to it at once), or `pinakesd catalogue import musicbrainz` with the
   daemon stopped. There is no such check under a `limit`.
3. The second pass writes only the cards that are new or changed; an unchanged card is not written.
   The text index takes the changes through its queue.
4. The listed cards are removed.

The cards are as current as the dump's `TIMESTAMP` (`current_as_of`). MusicBrainz publishes on
Wednesdays and Saturdays, and a dump's files appear about 15 hours after its `TIMESTAMP`, so the
cards fall up to about 4 days and 17 hours behind before the next update lands;
`CatalogueUpdateLag` allows five days.

A run cut off by a restart or a failure resumes after the last item it wrote, from the merged
runs once its read had finished them, so the dump is not read again. A read cut off keeps the files
it had read whole and checked (with a manifest each) and reads the others. When the dump a cut-off
run needs again is gone from the mirror, the run starts over on the newest dump. A run cut off in
its removals goes on with the rest of its list.

## A small instance

`[catalogues.musicbrainz] limit = N` imports at most N cards: the first releases of the dump whose
recordings, release groups and credited artists fit in N, then only those release groups and
artists. Recordings on no release are not read. The read stops once the releases are full, so a
small limit reads a few megabytes of the release file, but the release group and artist files are
read until every one wanted is found. A changed `limit` makes the next update a full import of the
new subset, which removes the cards outside it.

## Run it by hand

```sh
pinakesd --config <path> catalogue import musicbrainz [--from DIR|URL] [--limit N] [--update] [--force]
pinakesd --config <path> catalogue import musicbrainz --request
```

The first runs one full import (or with `--update` one update) now, through the daemon's code, with
the catalogue's lock: stop the daemon first, or switch the catalogue off there. A full import
started by hand takes up one that was cut off, else begins a new one.

- `--from` reads a local directory holding one dump's files, named after the dump
  (`20261125-001001`), with its `SHA256SUMS` when they are to be checked, or another mirror's
  `json-dumps/` URL (`https://`). The official mirror needs `http.contact`.
- `--limit` takes the place of the configured limit. Below the cards stored it would remove the
  others, so it needs `--force` then.

`--request` only asks for a full import: the daemon that runs the importer starts it after the run
under way (an update hands over at once), with its turn and disk checks.

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
