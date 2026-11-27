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
| Rate | `LATEST` polled once an hour (and asked again by the run it starts); one file at a time, streamed; nothing is kept but sorted runs under `catalogues.dir/musicbrainz` |
| Refresh | A new dump is read when `LATEST` names one; MusicBrainz publishes two a week |
| Needs | `http.contact`: without it the importer does not start (every request names the operator's contact); `catalogue status` then shows it in `waiting` and `pinakes_catalogue_needs_contact` is 1 |
| Card types | `Person` or `MusicGroup` (artists), `MusicAlbum` (release groups), `MusicRecording` (recordings) |
| Keys | `mb:artist:…`, `mb:release-group:…`, `mb:recording:…`, `isrc:…`, `wikidata:Q…`, `url:…` |
| Credit | `from: "musicbrainz"`; the card's link is its musicbrainz.org page |
| Switch | `[catalogues.musicbrainz] enabled`, on by default; `limit` for a small instance |

## What it reads

Each file is a `tar.xz` archive (of one xz stream or several, read as `xz` reads them) holding
one JSON line per entity (`mbdump/<entity>`), as MusicBrainz's web service writes it, and the
dump's `TIMESTAMP`. The importer streams them in this
order and keeps only what a card needs:

1. `release`: each release's artists, release group and recordings (with the release's date and
   its release group's title, date and genres);
2. `release-group`, `artist`: one entity each;
3. `recording`: the recordings on no release.

What belongs to one item (a recording on 200 releases) comes together in sorted runs under
`catalogues.dir/musicbrainz`, so the dump is never held in memory. The runs are merged down to at
most 256 before any card is written. All the runs of one read together (the stored cards' hashes
of an update included) hold at most the disk the read waited for, and the disk keeps 1 GiB free:
past either, the read stops (`disk`).

- **Checksums.** Each file read to its end is checked against the dump's `SHA256SUMS` before any
  card is written: a file whose SHA-256 differs, or one the list leaves out, stops the run
  (`checksum`; a file the mirror's list leaves out is not downloaded). A local dump without a
  `SHA256SUMS` is read unchecked, and so are the files a `limit` stops reading early.
- **Bounds.** A line longer than 128 MiB is skipped and counted. Each list of a line is read up to
  a cap (4,096 tracks a medium, 512 media, 2,048 relations, 256 aliases and genres, 64 ISRCs and
  credited artists) and the rest skipped; all the lists of one line together build at most
  500,000 entries. Under a `limit`, a release of more than 50,000 records counts as a bad line. Of one
  item's records at most 8,192 holding 64 MiB make its card (every one counts for its fame). The
  lines waiting for the parsing threads hold at most 64 MiB (one longer line is let through
  alone). A line that doesn't parse, or parses but names no valid MBID, is a bad line; more than
  100 bad lines, and more than one in a thousand of the lines read, stop the run (`parse`). A file
  whose xz stream or archive is broken stops it as `archive`.
- **Downloads.** A broken connection is asked for again from the byte where it broke (`Range`
  with `If-Range`, so a file replaced meanwhile is refused, never spliced), up to five times in a
  row (renewed after every 64 MiB read), and not at all when the mirror names no ETag or
  Last-Modified. A body that ends cleanly before the file's length (its first answer's
  `Content-Length`) is a break too; an answer that names no length is refused (`download`), as its
  early end could not be told from the whole. A file may take 12 hours.

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
| `isrc:<ISRC>` | A recording's ISRCs, at most eight, each only when no other recording of the dump carries it; none under a `limit` (an ISRC unique in the subset may be shared in the dump) |
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
2. A first pass lists the stored cards the dump no longer has.
3. The second pass writes only the cards that are new or changed; an unchanged card is not
   written. The text index takes the changes through its queue.
4. The listed cards are removed, and so are the cards whose items the dump now makes refused
   cards of (no name, say).

The cards are as current as the dump's `TIMESTAMP` (`current_as_of`). MusicBrainz publishes on
Wednesdays and Saturdays, and a dump's files appear about 15 hours after its `TIMESTAMP`, so the
cards fall up to about 4 days and 17 hours behind before the next update lands;
`CatalogueUpdateLag` allows five days.

### A dump that is refused

A dump is refused, and no card is removed for it, when:

- an update would remove more than a tenth of one kind of card (artists, release groups or
  recordings, each counted on its own: artists are about 6% of all cards), counted after the first
  pass, before any card is written, and again before the removals, counting the refused cards too
  (the new and changed cards are written by then);
- a full import of the importer's own (not asked for by hand, nor by an update handing over for a
  changed `limit`) would remove more than a tenth of one kind of card (one whose cards are mostly
  refused);
- a run (an update or a full import, even one asked for, with or without a `limit`) reads no
  record at all, or none from one file whose kind of card is stored (artists, release groups,
  recordings), while cards are stored (`empty`; `catalogue drop` empties a catalogue): one kind is
  a small share of the cards, below the removal check's tenth;
- its files don't match their checksums, don't parse, or hold more than a file may (`checksum`,
  `parse`, `archive`, `size`, `record_size`, `isrc_shared`).

The run then ends as a failure: `catalogue status` shows the phase `failed` and the reason with its
counts and the dump (`last_error`), `waiting` shows `dump <version> refused`,
`CatalogueImportFailed` fires for the hour after it, and `CatalogueDumpRefused` (from
`pinakes_catalogue_refused`) as long as the refusal stands, an hour on. The run is abandoned rather than resumed: its cursor and its sorted
runs go. The refused dump is not read again from the same source with the same `limit` (`refused`,
`refused_source` and `refused_limit` in the state's position): the importer waits for `LATEST` to
name another dump, which it reads after the usual disk check; another `limit` reads it again, as a
new subset. An update by hand (`--update`) of the refused dump fails at once (`refused`), and so
does a run whose `LATEST` names the refused dump again by the time it starts, unless a full import
was asked for. A full import refused at its end has written its new and changed cards by then, as
an update has; they stay until the next update corrects them. To read the refused dump again as
an update (a checksum failure while the mirror was still copying a file, say), clear the refusal
with the daemon stopped: `UPDATE catalogue_state SET position = position - 'refused' -
'refused_source' - 'refused_limit' WHERE catalogue = 'musicbrainz'`. If the dump is right, ask for a full import
of it, which removes every card it does not read: `pinakesd catalogue import musicbrainz --request`
beside the running daemon, or `pinakesd catalogue import musicbrainz` with the daemon stopped.
There is no removal check under a `limit`, nor for a full import asked for; one asked for and cut
off is still the one asked for when it resumes (`forced` in its cursor). A finished full import
clears the request. A `LATEST`
naming a dump dated after tomorrow is refused as no dump (`version`): a host clock more than a day
behind refuses every dump so (a first import fails on it at every try; once cards are stored, the
hourly poll logs it and `CatalogueUpdateLag` fires in time).

### Runs cut off

A run cut off by a restart or a failure resumes after the last item it wrote; one cut off in its
read resumes as the same run (its cursor is written when it starts). Once its read has
merged its runs, it resumes from them, so the dump is not read again; a read cut off keeps the
files it had read whole and checked (with a manifest each) and reads the others. A run cut off
after its writes reads nothing again. When the dump a cut-off run needs again is gone from the
mirror, the run starts over on the newest dump; a cut-off run of another source or `limit` (one by
hand with `--from` or `--limit`, say) is not resumed either: the run starts over. A full import that
starts over so begins a generation of its own, so the cards the dropped attempt wrote and it does
not read are removed at its end. An update cut off in its writes checks its list of removals again
(by kind, from the counts its first pass kept) before it writes on. A run cut off in its removals
goes on with the rest of its list.

## A small instance

`[catalogues.musicbrainz] limit = N` imports at most N cards: the first releases of the dump whose
recordings, release groups and credited artists fit in N, then only those release groups and
artists. Recordings on no release are not read. The read stops once the releases are full, so a
small limit reads a few megabytes of the release file, but the release group and artist files are
read until every one wanted is found. A changed `limit` makes the next update a full import of the
new subset, which removes the cards outside it. So does a `catalogue import --limit` by hand that
differs from the daemon's: the daemon's next update imports its own subset again.

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
- `--limit` takes the place of the configured limit. Another limit than the cards were read with,
  below the cards stored, would remove the others (an update hands over to a full import of the
  new subset), so it needs `--force` then, with `--update` too. `--force` also lets that import
  remove the other cards past the shrink floor (they are what the lower limit drops), when the
  limit is below both the cards held and the limit they were read with. A full
  import with `--force` and no lower `--limit` keeps the floor: a dump that reads under half the
  cards fails `shrunk`, and `pinakesd catalogue allow-shrink musicbrainz` lets it through.

`--request` only asks for a full import: the process running the importer starts it, with its
turn and disk checks, after an update under way or at its next step (within the hour when idle); a
full import under way is the one asked for, and its end (or its abandonment, for a refused dump)
clears the request: a try that fails or is cut off before its first cursor is still the one asked
for when it is tried again. The command says when
no process holds the catalogue.

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
