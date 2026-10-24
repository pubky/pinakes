# Discogs

Artists, masters and releases in the Discogs music database, read from its API
(`api.discogs.com`): Discogs answers requests for its web pages with a bot wall. No key; 25
requests a minute, the pace Pinakes keeps by default. Discogs' images need authentication: with a
personal access token (`[also_on.sources.discogs] token`, or
`PINAKES__ALSO_ON__SOURCES__DISCOGS__TOKEN`) the cards show them. With a token Discogs allows 60
requests a minute; an operator's `limits.host_rates` entry sets the pace outright
(`"api.discogs.com" = 1.0` under `[limits.host_rates]`). The token is sent only to
`api.discogs.com` and never appears in logs or errors. A token Discogs refuses (401 or 403: expired
or mistyped) is dropped: that request is asked again without it, later ones go without it from the start,
one warning is logged, and the cards are read without it (no images); `pinakes_source_token_refusals_total{source="discogs"}` counts each
refusal. Release data is CC0.

## Links

- an **artist**: `https://www.discogs.com/artist/{id}`;
- a **master**, an album across all its editions: `https://www.discogs.com/master/{id}`;
- a **release**, one edition (a pressing, a format, a country): `https://www.discogs.com/release/{id}`.

The name after the ID (`/master/21491-Radiohead-OK-Computer`), a language path (`/de/…`) and
`discogs.com` without `www` are aliases of that address. Labels and marketplace pages are not
read.

## What it reads

`GET https://api.discogs.com/{artists,masters,releases}/{id}` (2 to 50 KB). An ID Discogs does not
have is `missing`. The answer is shared for an hour with the also-on lookup of the same link.

## Card

An artist is a `MusicGroup` named without Discogs' number for namesakes (`Radiohead (2)` →
`Radiohead`); a `Person` when MusicBrainz says the artist is one. A master or release is a
`MusicAlbum` named by its title. A master is described by its artist credit and year
(`Radiohead · 1997`), a release also by its format and country
(`Radiohead · CD, Album, Stereo · Europe · 1997`). The image is Discogs' primary image, only with
a token.

| Fact | Meaning |
| --- | --- |
| `artist` | A master's or release's artist credit |
| `year` | The year it was released |
| `format`, `country` | A release's format and its descriptions; where it was released |
| `upc` | A release's barcodes, as 13 digits with a verified check digit |
| `master` | A release's master |

## Also on

A Discogs link joins other services' pages exactly, never by title or catalogue number:

- MusicBrainz names Discogs pages in its URL relations, at one level each: an artist's
  artist, a release group's master, a release's release. A master joins its release group, a
  release its one release (never the release group: an edition is not the album), an artist the
  artist.
- Wikidata's Discogs artist ID (P1953) and master ID (P1954) name the item of an artist or
  album; a release ID (P2206) names an item only when it is not an album's (it has no master and
  no MusicBrainz release group).
- A release's own barcode is a `upc:` key when it lists one; a box set lists each disc's, so
  several stay facts.

**Trust.** Discogs is edited by its users, so a release's barcode is what its editors entered.
Pinakes keys it on Discogs' word alone, as it keys MusicBrainz's barcodes (also edited by its
users). A wrong barcode joins this edition to another one's pages, and the join is a hint on the
card, not a claim. If wrong barcodes show up, the rule changes to keying a Discogs barcode only
when a second source names the same one, and keeping it as a fact otherwise.

Pages of other services list the Discogs artist, master or release that MusicBrainz or Wikidata
names for them at their level. A master's card lists the album's pages on other services: MusicBrainz
names none for a release group (its editions have them), so they come from the one Wikidata item
MusicBrainz relates the release group to, when that item names the release group back.

The `discogs` also-on source: a Discogs artist, master or release gets `discogs:{kind}:{id}` as key, and a release
up to four of its barcodes as facts, from the record its card reads (shared); a release that
lists one barcode also gets it as a `upc:` key (a box set lists each disc's), which
`deezer` and `itunes` then look up. Never by title or catalogue number.

Each Discogs release costs one Discogs request (the one its card makes), and a Discogs master one
Wikidata request (the item MusicBrainz relates its release group to). When Discogs' record fails,
the link's own key is recorded and the run comes back after a failed run's backoff
([operations](../operations.md#also-on)).

Keys: `discogs:{artist|master|release}:{id}`: each names one object, so a [group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds the links that carry it.

## Examples

- `https://www.discogs.com/master/21491-Radiohead-OK-Computer`
- `https://www.discogs.com/release/83182-Radiohead-OK-Computer`
- `https://www.discogs.com/artist/3840-Radiohead`
