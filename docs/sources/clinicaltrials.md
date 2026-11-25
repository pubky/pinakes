# ClinicalTrials.gov

Pinakes reads clinical studies registered at ClinicalTrials.gov (U.S. National Library of
Medicine) from its API (v2), by NCT number. No key is needed.

| | |
| --- | --- |
| Source | `clinicaltrials`, official source |
| Links | `clinicaltrials.gov/study/{NCT}`, the legacy `/show/{NCT}` and `/ct2/show/…/{NCT}` (also on `classic.`), `clinicaltrials.gov/api/v2/studies/{NCT}`; root `nct:{NCT}` |
| Fetches | `GET https://clinicaltrials.gov/api/v2/studies?filter.ids={NCT}&fields=protocolSection,hasResults&pageSize=2` |
| Credentials | None |
| Rate | 0.8 requests per second on `clinicaltrials.gov` |
| Refresh | Daily, as every official card; an ended trial with full dates becomes final after its end ([refresh](../design.md#refresh)) |
| Card types | `MedicalTrial` |
| Keys | None (facts only) |
| Credit | "ClinicalTrials.gov, data as of {date}", in `attribution` |
| Switch | `[sources.clinicaltrials] enabled`, on by default |

## Links

Every ClinicalTrials.gov address of one study is one resource. The root is `nct:{NCT}`, the NCT
number in upper case (`NCT` and 8 digits), such as `nct:NCT04368728`. These links are
[aliases](../api.md#aliases) of it:

- `https://clinicaltrials.gov/study/{NCT}` (also on `beta.clinicaltrials.gov`) and
  `https://clinicaltrials.gov/study/{NCT}/history`;
- `https://clinicaltrials.gov/show/{NCT}`;
- `https://clinicaltrials.gov/ct2/show/{NCT}` and `…/ct2/show/record/{NCT}`,
  `…/ct2/show/study/{NCT}` and `…/ct2/show/results/{NCT}`, also on `classic.clinicaltrials.gov`;
- `https://clinicaltrials.gov/api/v2/studies/{NCT}`.

The path decides: a query such as `?tab=results` or `?term=…` doesn't change the study. NCT numbers
are case-insensitive: a link or root in another letter case, such as `nct:nct04368728`, is an
alias of the upper-case root. An `nct:` root that is not a valid NCT number is `missing` at once
and never fetched.

A former number of a study, one the study lists among its aliases (`nctIdAliases`, for example
after two registrations were merged), reads that study. Its card is the study's, and the link
becomes an alias of the study's root.

Not read here: a past version of the record (`?tab=history&a={n}` or `/study/{NCT}/history/{n}`),
which is a dated version rather than the study; searches; other trial registries.

## What it fetches

One request per read:

`GET https://clinicaltrials.gov/api/v2/studies?filter.ids={NCT}&fields=protocolSection,hasResults&pageSize=2`

The request leaves out the results section, which can be megabytes. The search answers a former
number with its study, where `/api/v2/studies/{NCT}` would redirect to the study's full record.

An empty answer, for an NCT number the registry doesn't know, is `missing`. An answer for a study
that neither is nor was the number asked, or for more than one study, is an error, and the read is
retried. So is any status other than 200.

## Card

The type is `MedicalTrial`.

- `site` is "ClinicalTrials.gov", `name` the brief title (or else the official title) and
  `description` the brief summary, as the registry gives them. The card has no image.
- The `page_url` fact is the study's page, `https://clinicaltrials.gov/study/{NCT}`. Because the
  root `nct:{NCT}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "ClinicalTrials.gov, data as of {last update posted}", for example
  "ClinicalTrials.gov, data as of 2026-03-25", or "ClinicalTrials.gov" when the record gives no
  update date.

`start` and `end` are set only for a trial that has ended (`COMPLETED`, `TERMINATED` or
`WITHDRAWN`), and only from full dates (`YYYY-MM-DD`). Such a card becomes final once its end has
passed ([refresh](../design.md#refresh)). A running trial's dates, and a month alone (such as
`2010-01`), stay in the `start_date` and `completion_date` facts, so the card keeps being read
again.

| Fact | Meaning |
| --- | --- |
| `official_title` | The official title |
| `overall_status` | For example `RECRUITING`, `COMPLETED` |
| `start_date`, `start_date_type` | The start date as given, and `ACTUAL` or `ESTIMATED` |
| `completion_date`, `completion_date_type` | The completion date as given, and `ACTUAL` or `ESTIMATED` |
| `last_update_posted` | When the record was last updated (`YYYY-MM-DD`) |
| `study_type` | `INTERVENTIONAL`, `OBSERVATIONAL` or `EXPANDED_ACCESS` |
| `phases` | For example `PHASE2; PHASE3` |
| `enrollment` | The number of participants, actual or planned |
| `conditions`, `interventions` | What is studied, and the interventions by name |
| `sponsor` | The lead sponsor |
| `has_results` | Whether results are posted |
| `pmids` | Up to ten PubMed IDs of the study's references |
| `page_url` | The study's page on clinicaltrials.gov |

## Keys and groups

None. The PubMed IDs name other objects, the papers, and are facts only. The card joins no
[group](../api.md#groups).

## Credit

The `attribution` line credits ClinicalTrials.gov with the date of the data, which is the date the
record was last updated.

## Turn it off

```toml
[sources.clinicaltrials]
enabled = false
```

ClinicalTrials.gov links then get ordinary web cards, and `nct:` roots wait until the source is on
again.

## Examples

- `https://clinicaltrials.gov/study/NCT04368728` (the BioNTech/Pfizer COVID-19 vaccine trial)
