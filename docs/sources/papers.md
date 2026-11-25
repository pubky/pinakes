# `papers`: a paper's open copies, PubMed records and preprint

This lookup family of [also on](../api.md#also-on) runs on DOI cards and arXiv papers. It adds open
copies, PubMed and PubMed Central records, the arXiv preprint or journal version, and the paper's
Hugging Face page.

| | |
| --- | --- |
| Source | `papers`, lookup family with sources `unpaywall`, `ncbi`, `datacite`, `huggingface` |
| Runs on | `unpaywall`, `ncbi`: DOI cards. `datacite`: DOI cards and arXiv papers. `huggingface`: arXiv papers and cards with an `arxiv:` key |
| Asks | `https://api.unpaywall.org/v2/{DOI}`; the NCBI ID Converter (`https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/`); `https://api.datacite.org/dois`; `https://api.crossref.org/works/{DOI}`; `https://huggingface.co/api/papers/{id}` |
| Credentials | None. `unpaywall` and `ncbi` send the operator's email address (`[http] contact`) and find nothing without one |
| Rate | At most 3 requests per second on `pmc.ncbi.nlm.nih.gov`, 10 on `api.crossref.org`, and 1.6 on `huggingface.co` (the `huggingface` official source's limit); the shared default of 1 per second applies while it is lower. The other hosts: shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Rows live | 30 days; rows expire after twice that |
| Keys | `doi:{DOI}`, `arxiv:{id}`, `pmid:{n}`, `pmcid:PMC{n}` |
| Links | Repository copies, arXiv, alphaXiv, PubMed, PubMed Central, DOI, Hugging Face Papers. None is a `maybe`, and none carries a key |
| Facts | None |
| Switch | `[also_on.sources.<id>] enabled` for each of the four sources, on by default |

## What it does

A DOI card is a `doi:` link or a `doi.org` one. arXiv's own DOIs (`10.48550/arXiv.{id}`) are read as
arXiv papers, and any other DOI under `10.48550/` is read by no source here.

An arXiv paper is an `arxiv.org/abs/{id}` or `arxiv.org/pdf/{id}` link (also on `export.arxiv.org`,
with or without `.pdf`), an `alphaxiv.org/abs/{id}` link or an arXiv DOI. The version (`v2`) is
dropped. The old style (`hep-th/9901001`) and the new style (`2101.00001`) are read.

### `unpaywall`

1. The source asks Unpaywall for the card's DOI. A DOI with a `.` or `..` path segment is not asked.
   Without an operator email address it asks nothing.
2. Each location on a repository becomes a link. The publisher's own page does not.
3. A copy on arXiv or PubMed Central gets its canonical address. Any other link is the copy's
   landing page, else its own address, named after the repository's institution, else its host.
4. The link's `kind` is the version Unpaywall gives it: `submitted`, `accepted` or `published`.

### `ncbi`

1. The source asks the NCBI ID Converter for the card's DOI, with `tool=pinakes` and the operator's
   email address. It asks nothing without an email address, or for a DOI with a comma.
2. The answer must name the same DOI. The card gets its PubMed and PubMed Central pages as links, and
   the keys `pmid:` and `pmcid:`.

### `datacite`

On an arXiv paper:

1. The paper gets the key `arxiv:{id}` and its arXiv and alphaXiv pages as links, whatever the other
   services answer.
2. The source reads arXiv's DataCite record for the paper. When the record names exactly one journal
   version (`IsVersionOf`), the card gets that DOI as a link with `kind` `published`.
3. When the journal's publisher names this paper as its preprint (below), the card also gets the key
   `doi:{journal DOI}`.

On a DOI card:

1. The card gets the key `doi:{DOI}`, whatever the other services answer.
2. The source searches DataCite for arXiv records that name the DOI as their journal version. When
   exactly one does, and it names no other journal version, the card gets that paper's arXiv and
   alphaXiv pages as links with `kind` `submitted`. A DOI with a `"` or a `\` is not searched.
3. When the publisher names that paper as its preprint, the card also gets the key `arxiv:{id}`.

The publisher names a preprint in its own Crossref record: a `has-preprint` relation that the
publisher asserted (`asserted-by` `subject`), by arXiv ID or arXiv DOI. The source reads that record
only after DataCite has named a counterpart. A record longer than 1 MiB, and a DOI with a `.` or
`..` path segment, confirm nothing.

### `huggingface`

The source asks the Hugging Face API for the paper's arXiv ID. The card gets its Hugging Face Papers
page as a link only when the answer is for that ID. It also runs on a DOI card that already carries
an `arxiv:` key, from this run or an earlier one.

## Keys, links and facts

| Key | Source | Recorded when |
| --- | --- | --- |
| `doi:{DOI}` | `datacite` | It is the card's own DOI, or the journal DOI of an arXiv paper whose publisher names the paper |
| `arxiv:{id}` | `datacite` | It is the card's own arXiv ID, or the ID of a journal article's preprint whose publisher names the preprint |
| `pmid:{n}`, `pmcid:PMC{n}` | `ncbi` | The ID Converter's record is for the card's DOI |

Each key names one object, so a [group](../api.md#groups) holds at most one value of each. The
journal article and its preprint therefore join one group only when the publisher names the preprint.
DataCite records what the arXiv submitter entered, so its claim alone is a link and joins nothing.

Unpaywall matches copies itself, so its answers are links and never keys. [`key=`](../api.md#search) finds the
links that carry a key: `key=arxiv:2302.04668v2` drops the version, and `key=pmcid:` takes the ID
with its `PMC` prefix.

`site` is `arXiv`, `alphaXiv`, `PubMed`, `PubMed Central` or `Hugging Face Papers` only on a paper's
own page on that service.

## Cost

| Source | Requests per card |
| --- | --- |
| `unpaywall`, `ncbi` | One each |
| `datacite` | One to DataCite, and one to Crossref when DataCite names a counterpart |
| `huggingface` | One |

Answers are cached for `limits.discovery_cache_secs` (an hour by default).

When DataCite or Crossref fails, the card keeps its own key and what else was found, its stored rows
stay, and the run comes back after a failed run's backoff ([operations](../operations.md#runs)). A
failure of Unpaywall, the ID Converter or Hugging Face fails that source's run, which is retried.

## Turn it off

Switch off each source on its own:

```toml
[also_on.sources.unpaywall]
enabled = false

[also_on.sources.ncbi]
enabled = false

[also_on.sources.datacite]
enabled = false

[also_on.sources.huggingface]
enabled = false
```

After the restart, a source's stored keys and links stop showing, and groups leave its keys out.
They are kept until they expire and show again when the source is on. `[sources.huggingface]` is a
separate switch for the official source of the same name.
