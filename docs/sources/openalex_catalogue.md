# `openalex` catalogue: papers cited more than 50 times

This catalogue imports OpenAlex's snapshot: one [catalogue card](../operations.md#catalogues) for
each work cited more than `openalex.min_citations` times (50 by default, about 15 million
works). A shared DOI, arXiv, PubMed or OpenAlex link of such a paper gets its card from the
catalogue without a fetch.

| | |
| --- | --- |
| Catalogue | `openalex` |
| Reads | `https://openalex.s3.amazonaws.com/data/jsonl/manifest.json`, then the works files it lists (`data/jsonl/works/updated_date=YYYY-MM-DD/part_NNNN.gz`) |
| Credentials | None |
| Rate | The manifest once a day; eight works files at a time, streamed; nothing is written to disk |
| Refresh | A new snapshot is read when the manifest's date changes: only its files of newer dates |
| Needs | `http.contact`: without it the importer does not start (every request names the operator's contact) |
| Card types | `ScholarlyArticle`, `Book`, `Chapter`, `Dataset`, `Thesis`, `Report`, `CreativeWork` |
| Keys | `doi:…`, `openalex:W…`, `pmid:…`, `pmcid:PMC…`, `arxiv:…`, `url:…` |
| Credit | `from: "openalex"`; the card's link is its DOI or its openalex.org page |
| Switch | `[catalogues.openalex] enabled`, on by default; `min_citations` (50) |

## What it reads

The snapshot keeps works as gzipped JSON lines, one directory per `updated_date`. The manifest
names every file; a file outside the bucket's works tree is refused (`manifest`).

- **Order.** A work can sit in more than one date's directory; its newest copy is the right one.
  Files are read eight at a time in date order, and a file's cards are written only once every
  file of an earlier date is written. Files of one date are written side by side.
- **Filter.** A work is a card when it is cited more than `min_citations` times and is not front
  matter (`is_paratext`). A line whose highest `cited_by_count` is at or below the threshold is
  passed over without being parsed. A work that does not qualify removes its card if there is
  one, so a card written from an older copy goes when the newest copy no longer qualifies.
- **Bounds.** A line longer than 8 MiB is skipped and counted in the log; a card takes at most
  512 bytes of each text (title, venue, author). A file waiting for an earlier date holds at most
  50,000 cards back, the IDs of its works that don't qualify counting 32 to a card; past that its
  reader waits. A file may hold at most 16 GiB, the manifest 64 MiB and at most 100,000 files.
- **Downloads.** A broken connection is asked for again from the byte where it broke (`Range` with
  `If-Range`), up to five times in a row. A file may take 12 hours. The manifest lists no checksum
  of a works file; each is read as it arrives, and its gzip members' own CRC and length check it.

## Cards

| Field | Holds |
| --- | --- |
| `uri` | Its DOI when the work has one, stored as the root a DOI link is stored under (`doi:<doi>`, shared as `https://doi.org/<doi>`), else `https://openalex.org/W…` |
| `type` | From the work's type: `article`, `review`, `preprint`, `letter`, `editorial` and the rest are `ScholarlyArticle`; `book` is `Book`, `book-chapter` `Chapter`, `dataset` `Dataset`, `dissertation` `Thesis`, `report` `Report`, `reference-entry`, `standard` and `other` `CreativeWork` |
| `name` | Its title, without the markup OpenAlex keeps in some (`<i>`, `<sub>`) |
| `description` | Its year, type, venue and first three authors ("2022 · Article · Kidney International · A, B, C et al.") |
| `labels` | Its topics, their subfields and fields, and its keywords, as tag labels (one over 20 characters is left out), at most 32 |
| `fame` | `ln(1 + citations) / ln(100001)`, at most 1: 50 citations give 0.34, 1,000 give 0.60 |
| `details` | `cited_by`, `year`, `work_type`, `venue`, `authors`, `openalex` (its openalex.org page when the link is its DOI), `retracted` |

## Keys and groups

| Key | From |
| --- | --- |
| `doi:<doi in lower case>` | Its DOI |
| `openalex:W…` | Its OpenAlex ID |
| `pmid:<n>` | Its PubMed ID |
| `pmcid:PMC<n>` | Its PubMed Central ID |
| `arxiv:<id>` | An arXiv landing or PDF page among its locations, or an arXiv DOI (`10.48550/arxiv.…`) |

A shared link joins the card by the key it names by itself
([shared links](../operations.md#shared-links-a-catalogue-holds)): a DOI link, an arXiv abstract
or PDF page, a PubMed article page (`pubmed.ncbi.nlm.nih.gov/<n>/`) and an openalex.org work
page are the same paper.

**Works that share a DOI.** OpenAlex keeps some duplicate works that name the same DOI. Two cards
holding one DOI would tie, and a shared DOI link would join neither, so one card holds it: the most
cited (then the lowest ID). Each other one links its openalex.org page and drops the DOI's keys,
remembering them in `details.shared_doi`; it takes them back when it is the best holder left (the
holder removed, or cited less). This is decided at the end of each run, for the DOIs the run
wrote or removed a holder of (a full import, and an update taken up after it was cut off during
or after its removals: for all its cards). A card that moves counts as
changed, so the links already stored on its DOI are checked again and search reads it again.

## Updates

Each day the importer reads the manifest. When its date is new, it reads the works files of
dates after the last date it read, and that last date's files again when their list changed.
Changed cards are written, unchanged ones are not touched, and the works in those files that no
longer qualify are removed at the update's end. An update that would remove more than a tenth of
the cards (and more than ten) is refused, `refused`: nothing is removed, and the failure is
recorded once (`catalogue status` shows `refused: <snapshot>`, and
`pinakes_catalogue_refused{catalogue="openalex"}` is 1). The daily updates then end at once
without reading that snapshot again until a newer one appears, until a full import is asked for
(`pinakesd catalogue import openalex --request` with the daemon running, or
`catalogue import openalex` with it stopped), or until `pinakesd catalogue clear-refusal
openalex` clears it ([a refused dump or file](../operations.md#a-refused-dump-or-file)). A full
import whose snapshot holds no work (a manifest that lists no works file) is refused the same way
(`empty`). A line that may qualify but does not parse removes
nothing (its card stays); more than 100 such lines, and more than one in a thousand of a file's
lines, stop the run (`parse`). A line without a citation count is passed over.

OpenAlex's snapshot lists no merged or deleted works (its `merged_ids` lists ended in 2025 and
are no longer in `data/`), so an update can't see that a work went. A new snapshot that comes
180 days or more after the last full import is read by a full import instead of an update: the
works it no longer lists lose their cards. A changed `min_citations` also makes the next update
a full import. A full import whose manifest lists no works file is refused (`empty`) and removes
nothing. `current_as_of` is the snapshot's date; the
allowed lag is 100 days.

## Sample run

`scripts/catalogue-sample openalex` imports a few real works files (their first 100 MB each) into
a throwaway database, checks the cards, a search, a join and an update, and drops the database
([sample runs](../operations.md#catalogue-sample-runs)).

## Turn it off

`[catalogues.openalex] enabled = false`: the cards leave search at once, and DOI and paper links
are fetched as before.
