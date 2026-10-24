# `papers`: a paper's open copies, PubMed records and preprint

A lookup family of [also on](../api.md#also-on) (`lookups/papers.rs`).

A DOI card (and an arXiv paper):
- `unpaywall`: the open copies Unpaywall lists (repository and preprint copies, arXiv and PubMed
  Central at their canonical addresses), a repository copy with the version Unpaywall gives it as
  its `kind` (`submitted`, `accepted` or `published`); links only, since Unpaywall matches copies
  itself.
- `ncbi`: its PubMed and PubMed Central pages from the NCBI ID Converter, with `pmid:` and
  `pmcid:` keys.
- `datacite`: the arXiv preprint and its journal version where DataCite names them exactly
  (`IsVersionOf`, only when the record names one journal version): the journal card gets the
  arXiv and alphaXiv pages (`kind: "submitted"`), the arXiv paper its own pages and the journal
  version (`kind: "published"`), as links.
  A DOI card's own `doi:{DOI}` and an arXiv paper's own `arxiv:{id}` are their keys, recorded
  whatever DataCite and Crossref answer, so `key=doi:` finds the card. DataCite records what arXiv's submitter
  entered, so the two join (the journal card gets `arxiv:{id}`, the arXiv paper also
  `doi:{journal DOI}`) only when the journal's publisher names the preprint in its own Crossref
  record (`relation.has-preprint`, asserted by the publisher, by arXiv id or arXiv DOI). A
  preprint that names someone else's article stays a link and joins nothing, and so does one
  whose journal's Crossref record is too long to read (over 1 MB).
- `huggingface`: an arXiv paper's Hugging Face Papers page, only when its API knows the paper.

`unpaywall` and `ncbi` send the operator's contact (`[http] contact`) as their APIs ask; without
one they find nothing.


## Keys

`arxiv:{id}` (`datacite`), `pmid:{n}` and `pmcid:PMC{n}` (`ncbi`): each names one object, so a [group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds the links that carry it.

## Operations

Each DOI costs one request each to Unpaywall, the NCBI ID Converter and DataCite, and one to
Crossref when DataCite names its preprint (an arXiv paper one to DataCite, and one to Crossref
when DataCite names its journal version). Crossref is paced at 10 requests/s (its polite pool),
NCBI's ID Converter at 3/s (without an API key). When DataCite or Crossref fails, the link's own
key is recorded and the run comes back after a failed run's backoff
([operations](../operations.md#also-on)).
