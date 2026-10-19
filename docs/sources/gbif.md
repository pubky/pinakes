# GBIF

Species and occurrence records from the Global Biodiversity Information Facility, read from the
GBIF API by key. The gbif.org pages are behind a bot wall and are never read. No key; no
published rate limit (one request per second). The licence is per record.

## Links

`gbif:species/{key}` and `gbif:occurrence/{key}` are the roots (the key a positive number
without leading zeros). Every gbif.org address of the record is an alias of its root, with or
without `www.` and a language prefix (`/es/`, `/zh-tw/`, …):

- `https://www.gbif.org/species/{key}`, and its tabs `/metrics`, `/verbatim`, `/literature` and
  `/treatments`;
- `https://www.gbif.org/occurrence/{key}` and `…/verbatim`;
- `https://api.gbif.org/v1/species/{key}` and `https://api.gbif.org/v1/occurrence/{key}`.

A key GBIF does not know (404) is `missing`.

## What it reads

`GET https://api.gbif.org/v1/species/{key}` or `GET https://api.gbif.org/v1/occurrence/{key}`:
one request per card.

## Cards

A species (or any taxon of a GBIF checklist) is a `Taxon`, with `name` the scientific name with
its authorship. A synonym is its own record and names its accepted taxon. It has no place and
no date.

An occurrence (one record of an organism at a place and time) is an `Observation`, with `name`
the scientific name it was identified as.

- **Point:** the record's coordinates, except when the publisher withheld or generalised
  information (`informationWithheld`, `dataGeneralizations`, e.g. "Coordinate uncertainty
  increased to 27816m"), the coordinates are uncertain by more than 1,000 metres
  (`coordinateUncertaintyInMeters`), or GBIF flags them as invalid or mismatched. Then the card
  has no point.
- **Start:** `eventDate` when it is a full date or date-time. GBIF gives the time in local time
  with no offset; it is exact when `eventTime` carries the offset (as iNaturalist records do),
  otherwise the card keeps the date alone. A range of two full dates is a start and an end. A
  year or month alone stays in `event_date`.
- An occurrence published from iNaturalist (its `occurrenceID` an iNaturalist observation link)
  names that observation in the `inat_observation` fact.

| Fact | Meaning |
| --- | --- |
| `canonical_name`, `authorship` | The taxon's name without and its authorship |
| `rank`, `taxonomic_status` | E.g. `SPECIES`, `ACCEPTED` or `SYNONYM` |
| `kingdom`, `phylum`, `class`, `order`, `family`, `genus` | The classification |
| `vernacular_name` | The common name GBIF gives the taxon |
| `accepted_key`, `accepted_name` | A synonym's accepted taxon |
| `parent_key`, `published_in`, `dataset_key` | The parent taxon, the original publication, the checklist |
| `event_date` | The occurrence's date as GBIF gives it |
| `basis_of_record`, `occurrence_status` | E.g. `HUMAN_OBSERVATION`, `PRESENT` |
| `dataset_name`, `publisher` | The dataset and the publishing organisation's key |
| `recorded_by`, `country_code` | Who recorded it, where (ISO country code) |
| `taxon_key`, `taxon_rank` | The GBIF taxon it is identified as |
| `uncertainty_m` | Coordinate uncertainty in metres |
| `information_withheld`, `data_generalizations` | What the publisher withheld or generalised |
| `license`, `occurrence_id` | The record's licence (e.g. `CC BY 4.0`) and its publisher's id |
| `inat_observation` | The iNaturalist observation this occurrence was published from, by id |
| `page_url` | The record's page on gbif.org |

## Licence and attribution

Occurrences carry their own licence (CC0, CC BY 4.0 or CC BY-NC 4.0); the card's attribution is
the licence, the rights holder and "via GBIF.org", e.g. "CC BY-NC 4.0 (ramchop), via GBIF.org".
Species of the GBIF Backbone Taxonomy are credited "GBIF Backbone Taxonomy (CC BY 4.0), via
GBIF.org".

## Examples

- `https://www.gbif.org/species/1341976` (a taxon: the western honey bee)
- `https://www.gbif.org/occurrence/5938027305` (an observation)
