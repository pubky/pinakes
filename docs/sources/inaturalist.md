# iNaturalist

Observations and taxa from iNaturalist, read from the iNaturalist API (v1) by id. No key. The
API asks for at most 60 requests a minute and under 10,000 a day, so Pinakes sends one request
every 9 seconds. Licences are per observation and per photo.

## Links

`inat:observation/{id}` and `inat:taxon/{id}` are the roots (the id a positive number without
leading zeros). Every site of the iNaturalist Network shares one database, so the same address
on any of them is an alias of the root:

- `https://www.inaturalist.org/observations/{id}`;
- `https://www.inaturalist.org/taxa/{id}` and `…/taxa/{id}-{Name}`;
- `https://api.inaturalist.org/v1/observations/{id}` and `https://api.inaturalist.org/v1/taxa/{id}`.

The Network sites, as `/v1/sites` lists them: inaturalist.org, mexico.inaturalist.org (and
naturalista.mx), inaturalist.nz, inaturalist.ca, colombia.inaturalist.org, biodiversity4all.org,
inaturalist.ala.org.au, panama.inaturalist.org, ecuador.inaturalist.org, israel.inaturalist.org,
argentinat.org, costarica.inaturalist.org, inaturalist.mma.gob.cl, inaturalist.laji.fi,
inaturalist.se, spain.inaturalist.org, greece.inaturalist.org, guatemala.inaturalist.org,
uk.inaturalist.org, inaturalist.lu, taiwan.inaturalist.org and naturalista.uy, each with or
without `www.`. Observation links by UUID are not read (the API does not take them).

An id the API answers with no record for is `missing`.

## What it reads

`GET https://api.inaturalist.org/v1/observations/{id}` or `GET
https://api.inaturalist.org/v1/taxa/{id}`: one request per card.

## Cards

An observation is an `Observation`, with `name` the observer's species guess (else the taxon's
scientific name).

- **Point:** only when the observation is not obscured and neither the observer's geoprivacy nor
  the taxon's is `obscured` or `private`. iNaturalist's public coordinates for an obscured
  observation are randomised, so they are never used; the place guess is left out too.
- **Start:** the time observed, with its offset, else the date observed.
- **Image:** the first photo at medium size, only when the photo has a Creative Commons licence;
  its own credit is `image_attribution`.
- iNaturalist names the GBIF occurrence it published the observation as, in the
  `gbif_occurrence` fact.

A taxon is a `Taxon`, with `name` the scientific name, the default photo as `image` when it has a
Creative Commons licence, and `description` the Wikipedia summary iNaturalist shows (as text).

| Fact | Meaning |
| --- | --- |
| `observed_on` | The date observed |
| `quality_grade` | `research`, `needs_id` or `casual` |
| `taxon_id`, `taxon_name`, `taxon_rank`, `common_name` | The observation's taxon |
| `user_login` | The observer |
| `obscured`, `geoprivacy`, `taxon_geoprivacy` | Whether and why the location is hidden |
| `place_guess`, `positional_accuracy_m` | The place and accuracy, only when the location is public |
| `uuid` | The observation's UUID |
| `license_code` | The observation's licence, e.g. `cc-by-nc` |
| `photos`, `image_attribution` | The number of photos, the shown photo's credit |
| `rank`, `preferred_common_name`, `iconic_taxon_name` | A taxon's rank, common name and group |
| `observations_count` | Observations of the taxon |
| `is_active`, `extinct`, `parent_id` | Whether the taxon is current, extinct, its parent |
| `ancestry` | The ancestor taxa, from kingdom down |
| `wikipedia_url` | The taxon's Wikipedia article |
| `gbif_occurrence` | The GBIF occurrence iNaturalist published this observation as, by key |
| `page_url` | The page on inaturalist.org |

## Licence and attribution

An observation's attribution is the observer and its licence, in iNaturalist's form: "(c)
ramchop, some rights reserved (CC BY-NC), via iNaturalist" ("all rights reserved" without a
licence). A photo without a licence is never shown. A taxon card with a description is credited
"iNaturalist; description from Wikipedia (CC BY-SA 4.0)".

## Examples

- `https://www.inaturalist.org/observations/333069440` (an observation: a Brewer's blackbird)
- `https://www.inaturalist.org/taxa/47219-Apis-mellifera` (a taxon)
