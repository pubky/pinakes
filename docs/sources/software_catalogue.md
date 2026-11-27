# `software` catalogue: crates, F-Droid apps and Flathub applications

This catalogue imports three package registries' own catalogue files: one
[catalogue card](../operations.md#catalogues) for each crate on crates.io, each app on F-Droid
and each desktop or console application on Flathub. A shared package page gets its card from the
catalogue without a fetch.

| | |
| --- | --- |
| Catalogue | `software`, sources `crates.io`, `f-droid`, `flathub` |
| Reads | `https://static.crates.io/db-dump.tar.gz` (up to its `crates_keywords.csv`), `https://f-droid.org/repo/entry.json` and the `index-v2.json` it names, `https://dl.flathub.org/repo/appstream/x86_64/appstream.xml.gz` |
| Credentials | None |
| Rate | Each file's version once a day; a file when it is new; streamed, but for F-Droid's index, which is written to the catalogue's directory while it is checked |
| Refresh | Daily, each registry on its own |
| Needs | `http.contact` |
| Card types | `SoftwareSourceCode` (crates), `MobileApplication` (F-Droid), `SoftwareApplication` (Flathub) |
| Keys | `crate:…`, `android:…`, `flathub:…`, `url:…` |
| Credit | `from: "software"` with the registry as the card's source; the card's link is its package page |
| Switch | `[catalogues.software] enabled`, on by default |

## What it reads

- **crates.io.** The daily database dump, a gzipped tar of CSV files, read in order: categories,
  downloads, crates, keywords, the crates' categories and keywords. Reading stops after
  `crates_keywords.csv` (about 540 MB of the 2.3 GB file); the version tables after it are never
  read. Each crate's README is skipped as it is read. A dump without `crates.csv`, `keywords.csv`,
  `crates_categories.csv` or `crates_keywords.csv`, or one that ends inside a file, is refused
  (`archive`).
- **F-Droid.** `entry.json` names the index file with its SHA-256 and size; the index is written
  to a file in `catalogues.dir/software` while it is hashed and checked against both before it is
  parsed (`checksum`), so an index that is not the entry's is never parsed. The file's name is
  removed as soon as it is opened: its bytes go when the read ends, however it ends. crates.io and Flathub
  publish no checksum of their files: their integrity rests on TLS, the file's length and the gzip
  stream's own CRC.
- **Flathub.** The AppStream catalogue as a stream of XML elements: components of type
  `desktop-application`, `console-application` and `desktop`; runtimes, add-ons and
  localizations make no card. Texts in other languages (`xml:lang`) are passed over.
- **Bounds.** A CSV field keeps at most 2,048 bytes and a record at most 64 fields; a crate keeps
  a 1,024-byte description and links of at most 512 bytes (a longer one is left out), keywords
  and category names 64 bytes, and what the dump's reading holds (the crates and their
  downloads, categories and keywords) at most 1 GiB as counted (`size`); an XML text 4,096
  bytes, one XML event (a tag, a text, a comment) 1 MiB (`parse`), nesting 64 levels and 200,000
  components; F-Droid's index at most 512 MiB, Flathub's
  catalogue 256 MiB (512 MiB unpacked), the part of the crates.io dump read 16 GiB unpacked; each
  registry at most 2 million packages. A crate name, Android package or Flathub ID that is not
  one is left out, and so is a link that carries a credential. A dump file without the columns
  read is refused (`schema`).
- **F-Droid's index file.** The entry must name a plain file name (letters, digits, `.`, `_`,
  `-`) next to itself on the same host; anything else is refused (`parse`).

## Cards

| Field | crates.io | F-Droid | Flathub |
| --- | --- | --- | --- |
| `uri` | `https://crates.io/crates/<name>` | `https://f-droid.org/packages/<package>` | `https://flathub.org/apps/<id>` |
| `name` | The crate's name | Its English name | Its name |
| `description` | Its description | Its English summary | Its summary |
| `labels` | Its keywords and its categories' last part | Its categories | Its categories as words (`AudioVideo` is `audio-video`, toolkits left out) and its keywords |
| `related_uris` | Its repository and homepage | Its source code and website | Its homepage and source code |
| `fame` | `ln(1 + downloads) / ln(1 + 10^9)`, in steps of 0.01 (so a day's downloads rarely rewrite a card) | 0 | 0 |
| `details` | `repository`, `homepage`, `documentation`, `updated` | `license`, `author`, `added`, `updated`, `source_code`, `website` | `license`, `developer`, `homepage`, `source_code` |

F-Droid's and Flathub's catalogue files name no install count, so their fame is 0. A repository
is a related link, not a key: one repository can hold many crates.

## Keys and groups

The keys are those the package page names by itself ([shared
links](../operations.md#shared-links-a-catalogue-holds)): `crate:<name>` (crates.io, docs.rs and
lib.rs pages), `android:<package>` (F-Droid pages), `flathub:<id>` (Flathub pages). The
repository and homepage join the card's group without being built from it.

## Updates

Each day the importer asks for each file's version: the dump's and the AppStream file's ETag,
F-Droid's entry's time and digest. A registry whose file is new is read again whole: changed
cards are written, unchanged ones are not touched, and that registry's packages no longer listed
are removed. An update that would remove more than a tenth of a registry's cards (and more than
ten) is refused for that registry: nothing of it is removed, the other registries are still
updated, and the run's failure (`refused`) is recorded once (`catalogue status` shows
`refused: <registry> <version>`, and `pinakes_catalogue_refused{catalogue="software"}` is 1). The
daily updates then pass over the refused file until that registry has a newer one, until a full
import is asked for (`pinakesd catalogue import software --request` with the daemon running, or
`catalogue import software` with it stopped), or until `pinakesd catalogue clear-refusal
software` clears it ([a refused dump or file](../operations.md#a-refused-dump-or-file)). A full
import that reads no card of a registry refuses that registry's file the same way (`empty`) and
removes nothing; one that reads under half the cards a registry holds fails `shrunk` and removes
none of them (each registry has its own floor, besides the catalogue's), until `pinakesd
catalogue allow-shrink software` lets the next try through; a full import of the importer's own ends at once while a refused file is still
its registry's newest. A registry whose file can't be read (a download error)
leaves the others to be read too; the run then fails and is tried again. The cards are as
current as the newest of the three files (`current_as_of`; F-Droid publishes every few days);
the allowed lag is 2 days.

## Sample run

`scripts/catalogue-sample software` imports the first 300 MB of the real crates.io dump and the
real F-Droid and Flathub files into a throwaway database
([sample runs](../operations.md#catalogue-sample-runs)).

## Turn it off

`[catalogues.software] enabled = false`: the cards leave search at once, and package pages are
fetched as before.
