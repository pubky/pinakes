# Discogs

Pinakes reads artists, masters and releases in the Discogs music database from the Discogs API
(`api.discogs.com`), because Discogs' web pages answer automated requests with a bot wall. No
credential is needed. With one, the cards also show Discogs' images.

| | |
| --- | --- |
| Source | `discogs`, official source with an also-on half |
| Links | `discogs.com/artist/{id}`, `discogs.com/master/{id}`, `discogs.com/release/{id}` |
| Fetches | `GET https://api.discogs.com/artists/{id}`, `/masters/{id}` or `/releases/{id}` |
| Credentials | Optional `[also_on.sources.discogs] token`, or `key` and `secret`: images on cards |
| Rate | 25 requests a minute on `api.discogs.com` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `MusicGroup`, `Person`, `MusicAlbum` |
| Keys | `discogs:artist:…`, `discogs:master:…`, `discogs:release:…`, `upc:…` (from the [also-on half](#also-on)) |
| Credit | None |
| Switch | `[sources.discogs] enabled` (cards) and `[also_on.sources.discogs] enabled` (keys), both on by default |

## Links

Pinakes reads three kinds of Discogs object:

- an **artist**: `https://www.discogs.com/artist/{id}`;
- a **master**, an album across all its editions: `https://www.discogs.com/master/{id}`;
- a **release**, one edition (a pressing, a format, a country):
  `https://www.discogs.com/release/{id}`.

These are the roots. The name after the ID (`/master/21491-Radiohead-OK-Computer`), a language
path (`/de/…`, `/pt_BR/…`), `discogs.com` without `www`, `http` and a trailing slash are
[aliases](../api.md#aliases) of the root. An ID is up to 12 digits without a leading zero.

Not read here: labels, marketplace pages (`/sell/…`), image pages, links to `api.discogs.com`
and links with a port, user name or password.

## What it fetches

One request per read: `GET https://api.discogs.com/{artists|masters|releases}/{id}`. The answer
is shared for an hour with the also-on half's request for the same release, so they ask once.

- An ID Discogs doesn't have (404) is `missing`.
- A record whose `id` isn't the one asked for is an error, and gives no card.
- A 408, 429 or 5xx answer is retried later. Any other status is an error.

### Credentials

Discogs' images need a credential. Set either one in `[also_on.sources.discogs]`, which the card
route reads too (`[sources.discogs]` takes none):

| Credential | Config | Environment | Sent as |
| --- | --- | --- | --- |
| A personal access token | `token` | `PINAKES__ALSO_ON__SOURCES__DISCOGS__TOKEN` | `Authorization: Discogs token=…` |
| An app's consumer key and secret | `key` and `secret` | `PINAKES_DISCOGS_CONSUMER_KEY` and `PINAKES_DISCOGS_CONSUMER_SECRET` (used when the file leaves them unset) | `Authorization: Discogs key=…, secret=…` |

The token wins when both are set. The credential is sent only to `api.discogs.com` and never
appears in logs, errors or the operator info.

With a credential, Discogs allows 60 requests a minute, but Pinakes keeps its pace of 25 a
minute. To use the higher limit, set the pace yourself under `[limits.host_rates]`, for example
`"api.discogs.com" = 1.0`.

### A refused credential

When Discogs answers 401 or 403 to a request with the credential, Pinakes asks again without it.
If that second answer is the record (or a 404), the credential is what Discogs refused (expired,
revoked or mistyped):

- Pinakes stops sending it for an hour and logs one warning;
- cards are read without it meanwhile, so they have no images;
- `pinakes_source_token_refusals_total{source="discogs"}` counts each refusal.

A 401 or 403 that the request gets without the credential too is not a refusal of it.

## Card

- An artist is a `MusicGroup`, named without Discogs' number for namesakes (`Radiohead (2)` →
  `Radiohead`). It is a `Person` when MusicBrainz says the artist is one (the `artist_type` fact
  of `musicbrainz`).
- A master or a release is a `MusicAlbum` named by its title.
- `site` is "Discogs".
- `description`: for a master, the artist credit and year (`Radiohead · 1997`); for a release,
  also its format and country (`Radiohead · CD, Album, Stereo · Europe · 1997`).
- `image` is Discogs' primary image (else its first), only when the record was read with a
  credential.

| Fact | Meaning |
| --- | --- |
| `artist` | A master's or release's artist credit, each name as credited with its join word |
| `year` | The year it was released |
| `format` | A release's first format and its descriptions (`CD, Album, Stereo`) |
| `country` | Where a release was released, as Discogs names it (`Europe`) |
| `upc` | A release's barcodes, as GTIN-13 with a verified check digit, joined with `; ` |
| `master` | A release's master, as its link `https://www.discogs.com/master/{id}` |

## Keys and groups

The card route sets no key. A Discogs link's keys come from the [also-on half](#also-on) below,
and from the music hubs that name Discogs pages:

- `musicbrainz` relates each Discogs page at its own level: an artist to an
  artist, a master to a release group, a release to one release. A release never takes its
  release group's key, because an edition is not the album.
- `wikidata` finds the item that carries the Discogs ID: an artist (`P1953`), an
  album (`P1954`, a master), or an edition (`P2206`, a release). A release ID names an item only
  when the item is not an album's.

Pages of other services list the Discogs artist, master or release that MusicBrainz or Wikidata
names for them at their level. A master's card lists the album's pages on other services:
MusicBrainz names none for a release group (its editions have them), so they come from the one
Wikidata item MusicBrainz relates the release group to, when that item names the release group
back.

## Also on

The `discogs` also-on source records a Discogs link's own ID and a release's barcodes. It never
matches by title or catalogue number.

| | |
| --- | --- |
| Runs on | Discogs artist, master and release links |
| Asks | A release's record, `GET https://api.discogs.com/releases/{id}` (the card's request, shared); nothing for an artist or master |
| Rows live | 30 days; rows expire after 60 |
| Keys | `discogs:artist:{id}`, `discogs:master:{id}`, `discogs:release:{id}`, `upc:…` |
| Facts | `upc` |
| Switch | `[also_on.sources.discogs] enabled`, on by default |

- Every Discogs link gets the key `discogs:{kind}:{id}`. Each of these namespaces names one
  object, so a [group](../api.md#groups) holds at most one value of each. A search with
  `key=discogs:{kind}:{id}` finds the links that carry it.
- A release lists its barcodes as `upc` facts (at most four). A release that lists exactly one
  barcode also gets it as the key `upc:…`, which `deezer` and `itunes`
  then look albums up by. A box set lists each disc's barcode, so several barcodes stay facts and
  join nothing.

Discogs is edited by its users, so a release's barcode is what its editors entered. Pinakes keys
it on Discogs' word alone, as it keys MusicBrainz's barcodes. A wrong barcode joins this edition
to another one's pages, and the join shows as a link on the card.

When Discogs' record fails, the source records the link's own key and comes back after a failed
run's backoff ([operations](../operations.md#also-on)).

## Credit

The card credits nothing beyond its `site`, "Discogs".

## Turn it off

To stop reading Discogs cards:

```toml
[sources.discogs]
enabled = false
```

Discogs links then get ordinary web cards. To stop recording Discogs keys and barcodes:

```toml
[also_on.sources.discogs]
enabled = false
```

Its stored keys and facts stop showing at once, and Discogs links join groups only through the
music hubs' keys.

## Examples

- `https://www.discogs.com/master/21491-Radiohead-OK-Computer`
- `https://www.discogs.com/release/83182-Radiohead-OK-Computer`
- `https://www.discogs.com/artist/3840-Radiohead`
