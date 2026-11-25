# iNaturalist

Pinakes reads observations and taxa on iNaturalist from the iNaturalist API (v1), by ID. No key is
needed. Licences are per observation and per photo.

| | |
| --- | --- |
| Source | `inaturalist`, official source |
| Links | `{site}/observations/{id}` and `{site}/taxa/{id}` on any site of the iNaturalist Network, `api.inaturalist.org/v1/observations/{id}`, `api.inaturalist.org/v1/taxa/{id}`; roots `inat:observation/{id}`, `inat:taxon/{id}` |
| Fetches | `GET https://api.inaturalist.org/v1/observations/{id}` or `GET https://api.inaturalist.org/v1/taxa/{id}` |
| Credentials | None |
| Rate | 0.11 requests per second on `api.inaturalist.org`, about one every 9 seconds (the API asks for at most 60 requests a minute and under 10,000 a day) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Observation`, `Taxon` |
| Keys | None (facts only) |
| Credit | The observer and the observation's licence, or iNaturalist and Wikipedia for a taxon, in `attribution`; a photo's own credit in `image_attribution` |
| Switch | `[sources.inaturalist] enabled`, on by default |

## Links

Every address of one record is one resource. The roots are `inat:observation/{id}` and
`inat:taxon/{id}`. Every site of the iNaturalist Network shares one database, so the same address
on any of them is an [alias](../api.md#aliases) of the root:

- `https://www.inaturalist.org/observations/{id}`;
- `https://www.inaturalist.org/taxa/{id}` and `…/taxa/{id}-{Name}`, such as
  `/taxa/47219-Apis-mellifera`;
- `https://api.inaturalist.org/v1/observations/{id}` and `https://api.inaturalist.org/v1/taxa/{id}`.

The Network sites, each with or without `www.`, are the ones `/v1/sites` lists: inaturalist.org,
mexico.inaturalist.org (and naturalista.mx, which redirects to it), inaturalist.nz,
inaturalist.ca, colombia.inaturalist.org, biodiversity4all.org, inaturalist.ala.org.au,
panama.inaturalist.org, ecuador.inaturalist.org, israel.inaturalist.org, argentinat.org,
costarica.inaturalist.org, inaturalist.mma.gob.cl, inaturalist.laji.fi, inaturalist.se,
spain.inaturalist.org, greece.inaturalist.org, guatemala.inaturalist.org, uk.inaturalist.org,
inaturalist.lu, taiwan.inaturalist.org and naturalista.uy.

An ID is a positive decimal number of up to 12 digits. In a link, leading zeros name the same ID.
A root holds the ID without them: an `inat:` root that is not `inat:observation/{id}` or
`inat:taxon/{id}` with such an ID is `missing` at once and never fetched.

Not read here: observation links by UUID (the API doesn't take them), other pages of an
observation such as `/edit`, taxon links by name alone, photo pages, lists of IDs and API
versions other than v1.

## What it fetches

One request per read: `GET https://api.inaturalist.org/v1/observations/{id}` for an observation,
or `GET https://api.inaturalist.org/v1/taxa/{id}` for a taxon.

The API answers an unknown ID with no results (or a 404): the card is `missing`. An answer with
more than one record, or for another ID, is an error, and the read is retried.

## Cards

The `page_url` fact is the record's page on inaturalist.org,
`https://www.inaturalist.org/observations/{id}` or `https://www.inaturalist.org/taxa/{id}`.
Because the `inat:` root is not a web link, the API also lists that page in `open_in`.

### Observations

An observation is an `Observation`.

- `site` is "iNaturalist" and `name` the observer's species guess, or else the taxon's scientific
  name. The card has no description.
- **Point:** only when the record says the observation is not obscured, and both the observer's
  geoprivacy and the taxon's are unset or `open`. iNaturalist's public coordinates for an obscured or
  private location are randomized, so they are never used. The place guess and positional
  accuracy are left out too.
- **Time:** an observation has no `start`. It can still be obscured, re-identified or deleted, so
  its time is a fact (`observed_at`) and the card keeps being read again.
- **Image:** the first photo at medium size, only when the photo is in the public domain (`pd`) or
  under a Creative Commons licence (see [Credit](#credit)). Its own credit is the
  `image_attribution` fact.
- iNaturalist names the GBIF occurrence it published the observation as, which the
  `gbif_occurrence` fact holds.
- `attribution` credits the observer (their name, or else their login) and the observation's
  licence, in iNaturalist's form: "(c) ramchop, some rights reserved (CC BY-NC), via
  iNaturalist". Without a licence it says "all rights reserved"; under CC0 it is "ramchop, no
  rights reserved (CC0), via iNaturalist", without the "(c)".

### Taxa

A taxon is a `Taxon`.

- `site` is "iNaturalist" and `name` the scientific name.
- `description` is the Wikipedia summary iNaturalist shows, as plain text.
- `image` is the taxon's default photo, under the same licence rule as an observation's photo.
- `attribution` is "iNaturalist; description from Wikipedia (CC BY-SA 4.0)" when the card has a
  description, and "iNaturalist" otherwise.

### Facts

| Fact | Meaning |
| --- | --- |
| `observed_at` | The time observed (RFC 3339) |
| `observed_on` | The date observed |
| `quality_grade` | `research`, `needs_id` or `casual` |
| `taxon_id`, `taxon_name`, `taxon_rank`, `common_name` | The observation's taxon |
| `user_login` | The observer's login |
| `obscured`, `geoprivacy`, `taxon_geoprivacy` | Whether and why the location is hidden |
| `place_guess`, `positional_accuracy_m` | The place and its accuracy in meters, only when the location is public |
| `uuid` | The observation's UUID |
| `license_code` | The observation's licence code, for example `cc-by-nc` |
| `photos` | The number of photos |
| `image_attribution` | The credit of the photo the card shows |
| `rank`, `preferred_common_name`, `iconic_taxon_name` | A taxon's rank, common name and group |
| `observations_count` | The number of observations of the taxon |
| `is_active`, `extinct`, `parent_id` | Whether the taxon is current, whether it is extinct, and its parent |
| `ancestry` | The ancestor taxa, from kingdom down |
| `wikipedia_url` | The taxon's Wikipedia article |
| `gbif_occurrence` | The GBIF occurrence iNaturalist published this observation as, by key |
| `page_url` | The record's page on inaturalist.org |

## Keys and groups

None. `gbif_occurrence`, `taxon_id` and `parent_id` name other objects and are facts only. The
card joins no [group](../api.md#groups). The GBIF (`gbif`) card of the same occurrence names
this observation in its `inat_observation` fact.

## Credit

The `attribution` line credits the observer and the observation's licence, or iNaturalist (and
Wikipedia for the description) on a taxon card.

A photo is shown only in the public domain (`pd`) or under a Creative Commons licence: CC0, CC BY,
or CC BY with NC, ND or SA terms. Its credit is `image_attribution`. A photo with no licence code
or with any other one (such as `gfdl`) is not shown, and neither is one whose address is not an
`https` URL.

## Turn it off

```toml
[sources.inaturalist]
enabled = false
```

iNaturalist links then get ordinary web cards, and `inat:` roots wait until the source is on
again.

## Examples

- `https://www.inaturalist.org/observations/333069440` (an observation: a Brewer's blackbird)
- `https://www.inaturalist.org/taxa/47219-Apis-mellifera` (a taxon)
