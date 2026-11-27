# `tranco` catalogue: the homepages of the top million sites

This catalogue reads Tranco's daily list of the top million sites and then each site's homepage,
once, in the background: one [catalogue card](../operations.md#catalogues) for each homepage that
answers with a page (about 650,000). A shared homepage link gets its card from the catalogue
without a fetch.

| | |
| --- | --- |
| Catalogue | `tranco` |
| Reads | `https://tranco-list.eu/top-1m-id`, then `https://tranco-list.eu/download/<id>/1000000`; each site's `robots.txt` and homepage through the [egress](../design.md) |
| Credentials | None |
| Rate | The list once a day; homepages 16 at a time, for at most an hour per run, each host at its own pace |
| Refresh | A new list daily; each homepage again after 90 days |
| Needs | `http.contact` |
| Card type | `WebSite` |
| Keys | `homepage:<site>`, `url:…` |
| Credit | `from: "tranco"`; the card's link is `https://<site>/` |
| Switch | `[catalogues.tranco] enabled`, on by default |

## What it reads

- **The list.** `rank,site` lines; a line that is no rank and host name is passed over. Each
  site goes into `catalogue_site` with its rank and the day of the last list that named it (both
  moved at most weekly, so a daily list rewrites few rows). A rank past 2,000,000 is passed over,
  and a list's day is never later than the time it is read. A new site is due at once.
  Tranco publishes no checksum of a list: its integrity rests on TLS and the lines' own checks.
- **Homepages.** Sites are read best ranked first, 16 at a time (a new read starts as one ends),
  through the same egress as every fetch: pinned DNS, no private address, each host's pace, the
  body caps. First `https://<site>/robots.txt` (RFC 9309): when its rules for `pinakes`, else for
  `*`, don't allow `/`, the homepage is not read. A missing file (a 4xx) allows; a file over
  512 KiB counts as a no, and so do the rules for an agent past 1,000 or with a pattern over 512
  bytes (a rule left out could be the one that says no); a 429, a timeout, a server error or no
  answer is tried again the next day (not before its `Retry-After`). When the page is deferred
  after its robots.txt (the file took the host's turn), the rules are kept for a day per origin
  (scheme, host and port; 2,000 origins at most, the oldest goes first), so its next try reads
  the page without asking the file again. Then the homepage: a
  2xx HTML page that is no bot wall (the web route's own wall check) and that ends on the same
  site makes the card. Redirects are followed by hand, at most five, each on the same site
  (registrable domain), and each new host's robots.txt is asked before its page is read. A list entry that is an address, not a name, is passed over. A read waits at most two seconds for its host's or address's turn at the
  egress, so it gives way to the daemon's other fetches; one not given its turn is tried again
  an hour later (or when the egress's pause for that site ends, if later), without counting as a
  failure. A 429, a 408 or a 5xx is tried again the next day or after its `Retry-After`,
  whichever is later (at most a week; the egress holds a 429's to an hour), and counts as a
  failure.
- **When.** Reads run after the bulk imports: they wait while a large catalogue's full import
  (Wikidata, MusicBrainz, OpenStreetMap, OpenAlex) runs, and while a large catalogue that is on
  and has an importer in this build (all four) has not finished its first full
  import. Meanwhile the day's list ID is asked at most once a day, the run waits ten minutes,
  `catalogue status` says which catalogue the reads wait for, and
  `pinakes_catalogue_homepage_reads_waiting` is 1. A run reads for at most an hour; while
  homepages are still due, the next run starts at once. Once none is due, the next run starts
  when the next site falls due (a read deferred an hour, a retry the next day, a 90-day re-read),
  at least ten minutes and at most a day after the last run.

| Outcome | Card | Read again |
| --- | --- | --- |
| `ok` | Written | 90 days |
| `robots`, `not_html`, `status` (a 4xx), `wall` (a site without a card) | Removed | 90 days |
| `wall` in front of a site that has a card (a challenge may pass) | Kept | The next day; after three in a row, removed and 90 days |
| `busy` (a 429, 408 or 5xx, of the page or its robots.txt), `unreached` | Kept | The next day, or after its `Retry-After` (at most a week) when later; after three in a row, removed and 90 days |
| `other` (a redirect off the site, more than five redirects, or a page without a name) | Removed | 90 days |
| `deferred` (the egress gave the read no turn within two seconds, or the site's pause runs: nothing was read) | Kept, outcome unchanged | An hour later, or when the egress's wait ends |

`pinakes_catalogue_homepage_reads_total{outcome}` counts the reads by outcome;
`pinakes_catalogue_backlog{queue="homepages"}` the sites due now, and
`pinakes_catalogue_backlog_age_seconds{queue="homepages"}` how long the oldest has been due (a
backlog that only grows: reads waiting for a large import, or every read deferred; the count
stops at 100,000). Alert `CatalogueHomepageReadsStalled` fires when no read has found a page for
six hours while sites are due and the reads do not wait for a large import.
`catalogue status` shows the list and how many sites have been read (`upstream_version`).

## Cards

| Field | Holds |
| --- | --- |
| `uri` | `https://<site>/` |
| `name`, `description` | The page's name (its title or its own metadata) and description, as the web route reads them |
| `fame` | `1 - ln(rank) / ln(1000001)` at the time of the read |
| `details` | `rank` |

## Keys and groups

The key is `homepage:<site without www.>`: a shared link to a site's homepage (`https` or `http`,
with or without `www.`, path `/` and no query) joins its card
([shared links](../operations.md#shared-links-a-catalogue-holds)).

## Updates

Once a day a run asks for the day's list ID (the runs in between read what fell due from the
list it has); a new list is loaded, and sites off the list for a month
(30 days after the last weekly move of their day) lose their card and their row. A run never
removes more than a tenth of the cards (and more than ten): a list that would leave more off it,
or a round of homepage reads that would take the run's removals past it (every read failing), is
refused (`refused`, nothing of the list's removals or of that round written, its sites due again,
[a refused dump or file](../operations.md#a-refused-dump-or-file)), and nothing is read until the
next day's list, or `pinakesd catalogue clear-refusal tranco` (which checks again). A full import
asked for (`pinakesd catalogue import tranco --request`) has no such cap: it removes what its list
and reads say, however many, so a list that stays short (the sites off it for good) goes through
that way. When the list can't be looked up (tranco-list.eu down), an update logs it and reads the
homepages due on the list it has (nothing while a refusal stands); the list is asked again at the
next run. The cards are
as current as the last list loaded (its `Last-Modified` time); the allowed lag is 2 days. `pinakesd catalogue drop tranco`
also deletes `catalogue_site`.

## Run it by hand

`pinakesd catalogue import tranco [--from FILE] [--limit N] [--update]` runs one import or
update now. It reads homepages through an egress of its own, which does not share the running
daemon's per-host pace: stop the daemon (or switch Tranco off in it) first, so no host is asked
twice as fast from one address.

## Sample run

`scripts/catalogue-sample tranco` loads the real list and reads at most 50 homepages in all (44,
then 1 in the update and 5 from the official list) into a throwaway database ([sample runs](../operations.md#catalogue-sample-runs)).

## Turn it off

`[catalogues.tranco] enabled = false`: the cards leave search at once, no homepage is read, and
homepage links are fetched as before.
