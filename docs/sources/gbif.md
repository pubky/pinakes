# GBIF

Pinakes reads species and occurrence records of the Global Biodiversity Information Facility from
the GBIF API, by key. No key is needed. The licence is per record.

| | |
| --- | --- |
| Source | `gbif`, official source |
| Links | `gbif.org/species/{key}` and its tabs, `gbif.org/occurrence/{key}` and its `verbatim` tab, with or without a language prefix, `api.gbif.org/v1/species/{key}`, `api.gbif.org/v1/occurrence/{key}`; roots `gbif:species/{key}`, `gbif:occurrence/{key}` |
| Fetches | `GET https://api.gbif.org/v1/species/{key}` or `GET https://api.gbif.org/v1/occurrence/{key}` |
| Credentials | None |
| Rate | 1 request per second on `api.gbif.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Taxon`, `Observation` |
| Keys | None (facts only) |
| Credit | Per record, ending in "via GBIF.org", in `attribution`: an occurrence's rights holder, dataset and licence; the GBIF Backbone Taxonomy for its taxa |
| Switch | `[sources.gbif] enabled`, on by default |

## Links

Every address of one record is one resource. The roots are `gbif:species/{key}` and
`gbif:occurrence/{key}`, and these links are [aliases](../api.md#aliases) of them:

- `https://www.gbif.org/species/{key}` and its tabs `/metrics`, `/verbatim`, `/literature` and
  `/treatments`;
- `https://www.gbif.org/occurrence/{key}` and `…/verbatim`;
- `https://api.gbif.org/v1/species/{key}` and `https://api.gbif.org/v1/occurrence/{key}`.

The gbif.org links count with or without `www.`, and with or without a language prefix such as
`/es/` or `/zh-tw/` (two lower-case letters, optionally a hyphen and two more).

A key is a positive decimal number of up to 19 digits. In a link, leading zeros name the same key.
A root holds the key without them: a `gbif:` root that is not `gbif:species/{key}` or
`gbif:occurrence/{key}` with such a key is `missing` at once and never fetched.

Not read here: other species pages (such as `/species/{key}/children` and searches), datasets,
other API paths (such as `/v1/species/{key}/vernacularNames`) and national GBIF portals such as
`gbif.es`.

## What it fetches

One request per read: `GET https://api.gbif.org/v1/species/{key}` for a species, or
`GET https://api.gbif.org/v1/occurrence/{key}` for an occurrence. A key GBIF doesn't know (404) is
`missing`. An answer for another key is an error, and the read is retried.

## Cards

Neither card has a description or an image. The `page_url` fact is the record's page,
`https://www.gbif.org/species/{key}` or `https://www.gbif.org/occurrence/{key}`. Because the
`gbif:` root is not a web link, the API also lists that page in `open_in`.

### Species

A species, or any other taxon of a GBIF checklist, is a `Taxon`. It has no place and no date.

- `site` is "GBIF" and `name` the scientific name with its authorship.
- A synonym is its own record and names its accepted taxon in `accepted_key` and `accepted_name`.
- `attribution` is "GBIF Backbone Taxonomy (CC BY 4.0), via GBIF.org" for a taxon of the GBIF
  Backbone Taxonomy, and "via GBIF.org" for a taxon of another checklist.

### Occurrences

An occurrence, one record of an organism at a place and time, is an `Observation`.

- `site` is "GBIF" and `name` the scientific name it was identified as.
- **Point:** the record's coordinates. The card has no point when the publisher withheld or
  generalized information (`informationWithheld`, `dataGeneralizations`, for example "Coordinate
  uncertainty increased to 27816m"), when the coordinates are uncertain by more than 1,000 meters
  (`coordinateUncertaintyInMeters`), or when GBIF flags geospatial issues (`hasGeospatialIssues`,
  or a zero, invalid or out-of-range coordinate, or one that doesn't match the country).
- **Time:** an occurrence has no `start`. Its record can still be re-identified or withdrawn, so its
  time is a fact and the card keeps being read again. `observed_at` is `eventDate` as a time when it
  is one full date or date-time. GBIF gives a date-time in local time with no offset. The time is
  exact when `eventTime` carries the offset (as records from iNaturalist do); otherwise
  `observed_at` is the date at midnight UTC. A range, or a year or month alone, stays in
  `event_date` only.
- An occurrence published from iNaturalist (its `occurrenceID` is an iNaturalist observation link)
  names that observation's ID in the `inat_observation` fact.
- `attribution` is the rights holder (or else the recorder), the dataset and the licence, then
  "via GBIF.org", for example "ramchop, iNaturalist research-grade observations, CC BY-NC 4.0, via
  GBIF.org". With none of the three, it is "via GBIF.org".

### Facts

| Fact | Meaning |
| --- | --- |
| `canonical_name`, `authorship` | A taxon's name without its authorship, and the authorship |
| `rank`, `taxonomic_status` | For example `SPECIES`, and `ACCEPTED` or `SYNONYM` |
| `kingdom`, `phylum`, `class`, `order`, `family`, `genus` | The classification |
| `vernacular_name` | The common name GBIF gives the taxon |
| `accepted_key`, `accepted_name` | A synonym's accepted taxon |
| `parent_key`, `published_in` | The parent taxon and the original publication |
| `dataset_key` | The checklist or dataset the record belongs to |
| `observed_at` | When it was observed (RFC 3339), for one full date or date-time |
| `event_date` | The occurrence's date as GBIF gives it |
| `basis_of_record`, `occurrence_status` | For example `HUMAN_OBSERVATION`, `PRESENT` |
| `dataset_name`, `publisher` | The dataset's name and the publishing organization's key |
| `recorded_by`, `country_code` | Who recorded it, and where (ISO country code) |
| `taxon_key`, `taxon_rank` | The GBIF taxon it is identified as, and its rank |
| `uncertainty_m` | The coordinate uncertainty, in meters |
| `information_withheld`, `data_generalizations` | What the publisher withheld or generalized |
| `license` | The occurrence's licence, as a short name for a Creative Commons licence (for example `CC BY 4.0`, `CC0 1.0`) or as given |
| `occurrence_id` | The publisher's ID for the occurrence |
| `inat_observation` | The iNaturalist observation this occurrence was published from, by ID |
| `page_url` | The record's page on gbif.org |

## Keys and groups

None. `inat_observation`, `taxon_key` and `accepted_key` name other objects and are facts only.
The card joins no [group](../api.md#groups). The iNaturalist (`inaturalist`) card of the same
observation names this occurrence in its `gbif_occurrence` fact.

## Credit

The `attribution` line credits what the record states. An occurrence carries its own licence,
which `license` holds, and its rights holder. A taxon of the GBIF Backbone Taxonomy is credited to
that checklist under CC BY 4.0.

## Turn it off

```toml
[sources.gbif]
enabled = false
```

GBIF links then get ordinary web cards, and `gbif:` roots wait until the source is on again.

## Examples

- `https://www.gbif.org/species/1341976` (a taxon: the western honey bee)
- `https://www.gbif.org/occurrence/5938027305` (an observation)
