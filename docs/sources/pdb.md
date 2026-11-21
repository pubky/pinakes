# RCSB PDB

Structures in the Protein Data Bank, read from the RCSB PDB Data API by entry id. CC0; no key; no
published rate limit (one request per second).

## Links

`pdb:{ID}` and every RCSB address of an entry are one resource, with `pdb:{ID}` as the root (the
four-character id in upper case, e.g. `pdb:4HHB`):

- `https://www.rcsb.org/structure/{id}` and the entry's other tabs: `/3d-view/{id}`,
  `/sequence/{id}`, `/annotations/{id}`, `/experimental/{id}`, `/versions/{id}`;
- the legacy `https://www.rcsb.org/pdb/explore.do?structureId={id}` (and
  `/pdb/explore/explore.do?…`);
- `https://data.rcsb.org/rest/v1/core/entry/{id}`.

Ids are case-insensitive: `pdb:{id}` in another letter case (`pdb:4hhb`) is an alias of the root.
The extended form `pdb_0000{id}` (e.g. `pdb_00004hhb`) is the same
entry. Coordinate files (`files.rcsb.org`) are files of an entry, not the entry, and are not read.

An id the API does not know is `missing`; so is an obsolete entry (the API answers it with 404 as
well).

## What it reads

`GET https://data.rcsb.org/rest/v1/core/entry/{ID}`.

## Card

`Dataset`, with `site` "RCSB PDB", `name` the structure's title (as deposited, often in capitals),
and the attribution "RCSB PDB (CC0 1.0)". A structure is not an event, so its dates are facts.

| Fact | Meaning |
| --- | --- |
| `doi` | The entry's own DOI (`10.2210/pdb{id}/pdb`), when the entry states it |
| `released`, `deposit_date`, `revision_date` | The initial release, deposit and last revision, `YYYY-MM-DD` |
| `methods` | Experimental methods, e.g. `X-RAY DIFFRACTION` |
| `resolution` | In ångströms |
| `polymer_entity_count` | Distinct polymer entities |
| `molecular_weight_kda` | The deposited model's weight, in kDa |
| `classification` | The PDB classification, e.g. `OXYGEN TRANSPORT` |
| `keywords` | The entry's keywords |
| `citation_title`, `citation_journal`, `citation_year` | The primary citation |
| `citation_doi`, `citation_pubmed_id` | The primary citation's ids (another object) |
| `page_url` | The entry's page on rcsb.org |

## Examples

- `https://www.rcsb.org/structure/4HHB` (human deoxyhaemoglobin)
- `https://www.rcsb.org/structure/6VXX` (SARS-CoV-2 spike glycoprotein)
