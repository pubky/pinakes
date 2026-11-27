# OpenAlex catalogue fixtures

- `works.jsonl`: seven work records of the OpenAlex snapshot of 2026-10-14
  (`https://openalex.s3.amazonaws.com/data/jsonl/works/updated_date=2026-10-14/part_0158.gz`, from
  its first 15 MB): five cited more than 50 times (with a PubMed ID, with an arXiv page, without a
  DOI, a book chapter, a review), one cited exactly 50 times and one cited three times. Their long
  lists (abstract index, references, related works, authorships, concepts, locations, yearly
  counts) are cut to their first entries; every other field is as published.
- `manifest.json`: the snapshot's `data/jsonl/manifest.json` of 2026-10-14 with its first three and
  last three works files and one authors file.

OpenAlex's snapshot is CC0.
