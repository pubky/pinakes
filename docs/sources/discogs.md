# Discogs

Artists, masters and releases in the Discogs music database, read from its API
(`api.discogs.com`): Discogs answers requests for its web pages with a bot wall. No key; 25
requests a minute, the pace Pinakes keeps. Discogs' images need authentication, so the cards
have none. Release data is CC0.

## Links

- an **artist**: `https://www.discogs.com/artist/{id}`;
- a **master**, an album across all its editions: `https://www.discogs.com/master/{id}`;
- a **release**, one edition (a pressing, a format, a country): `https://www.discogs.com/release/{id}`.

The name after the ID (`/master/21491-Radiohead-OK-Computer`), a language path (`/de/…`) and
`discogs.com` without `www` are aliases of that address. Labels and marketplace pages are not
read.

## What it reads

`GET https://api.discogs.com/{artists,masters,releases}/{id}` (2 to 50 KB). An ID Discogs does not
have is `missing`.

## Card

An artist is a `MusicGroup` named without Discogs' number for namesakes (`Radiohead (2)` →
`Radiohead`). A master or release is a `MusicAlbum` named by its title. A master is described
by its artist credit and year (`Radiohead · 1997`), a release also by its format and country
(`Radiohead · CD, Album, Stereo · Europe · 1997`).

| Fact | Meaning |
| --- | --- |
| `artist` | A master's or release's artist credit |
| `year` | The year it was released |
| `format`, `country` | A release's format and its descriptions; where it was released |
| `upc` | A release's barcodes, as 13 digits with a verified check digit |
| `master` | A release's master |

## Examples

- `https://www.discogs.com/master/21491-Radiohead-OK-Computer`
- `https://www.discogs.com/release/83182-Radiohead-OK-Computer`
- `https://www.discogs.com/artist/3840-Radiohead`
