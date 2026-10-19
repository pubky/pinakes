# ClinicalTrials.gov

Clinical studies registered at ClinicalTrials.gov (U.S. National Library of Medicine), read from
its API (v2) by NCT number. Public domain; no key; no published rate limit (one request per
second).

## Links

`nct:{NCT number}` is the root (`NCT` and 8 digits, in upper case, e.g. `nct:NCT04368728`).
Every ClinicalTrials.gov address of the study is an alias of it, whatever its query (`?tab=…`,
`?term=…`):

- `https://clinicaltrials.gov/study/{NCT}` and `…/study/{NCT}/history` (also on `beta.`);
- `https://clinicaltrials.gov/show/{NCT}`;
- `https://clinicaltrials.gov/ct2/show/{NCT}` and `…/ct2/show/record|study|results/{NCT}`, also on
  `classic.clinicaltrials.gov`;
- `https://clinicaltrials.gov/api/v2/studies/{NCT}`.

An NCT number the registry does not know (404) is `missing`.

## What it reads

`GET https://clinicaltrials.gov/api/v2/studies/{NCT}?fields=protocolSection,hasResults` (the
results section is left out: it can be megabytes).

## Card

`MedicalTrial`, with `name` the brief title and `description` the brief summary, as the registry
gives them. `start` and `end` are the start and completion dates when they are full dates; a
month alone (e.g. `2010-01`) stays in `start_date` and `completion_date`. The card links to the
same study in the WHO International Clinical Trials Registry Platform.

| Fact | Meaning |
| --- | --- |
| `official_title` | The official title |
| `overall_status` | E.g. `RECRUITING`, `COMPLETED` |
| `start_date`, `start_date_type` | As given, and `ACTUAL` or `ESTIMATED` |
| `completion_date`, `completion_date_type` | As given, and `ACTUAL` or `ESTIMATED` |
| `last_update_posted` | When the record was last updated (`YYYY-MM-DD`) |
| `study_type` | `INTERVENTIONAL`, `OBSERVATIONAL` or `EXPANDED_ACCESS` |
| `phases` | E.g. `PHASE2; PHASE3` |
| `enrollment` | Participants, actual or planned |
| `conditions`, `interventions` | What is studied, and the interventions by name |
| `sponsor` | The lead sponsor |
| `has_results` | Whether results are posted |
| `pmids` | Up to ten PubMed ids of the study's references |
| `page_url` | The study's page on clinicaltrials.gov |

## Attribution

ClinicalTrials.gov asks to be credited with the date of the data: the card's attribution is
"ClinicalTrials.gov, data as of {last update posted}", e.g. "ClinicalTrials.gov, data as of
2026-03-25".

## Examples

- `https://clinicaltrials.gov/study/NCT04368728` (the BioNTech/Pfizer COVID-19 vaccine trial)
