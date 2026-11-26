# MusicBrainz JSON dump records

Real lines from MusicBrainz's JSON dump `20261125-001001`
(`https://data.metabrainz.org/pub/musicbrainz/data/json-dumps/20261125-001001/`), read on
2026-11-25 from the first 4 MB of each file (`<entity>.tar.xz`, `mbdump/<entity>`), one entity per
line as in the dump:

- `release.jsonl`: three releases: two editions of Tori Amos' "A Sorta Fairytale" that share their
  recordings and release group, and Arovane's "Cycliph" (recordings with ISRCs and streaming
  links, a release with a YouTube Music album link and a Discogs release);
- `release-group.jsonl`: two release groups with Wikidata, Discogs master and Wikipedia links;
- `artist.jsonl`: two artists with streaming, Bandcamp, Discogs, Wikidata and homepage links;
- `recording.jsonl`: two recordings on no release.

`tests/catalogue_musicbrainz.rs` packs them into dumps (`<entity>.tar.xz` with the dump's
`TIMESTAMP`, and `SHA256SUMS`) as the importer reads them.

The fields `tags`, `rating` and `annotation` were taken out. MusicBrainz's core data is CC0;
genres are MusicBrainz's supplementary data, CC BY-NC-SA 3.0
(<https://musicbrainz.org/doc/About/Data_License>).
