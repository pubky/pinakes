# RCSB PDB

Pinakes reads structures in the Protein Data Bank from the RCSB PDB Data API, by entry ID. No key
is needed.

| | |
| --- | --- |
| Source | `pdb`, official source |
| Links | `rcsb.org/structure/{id}` and the entry's other tabs, the legacy `rcsb.org/pdb/explore.do?structureId={id}`, `data.rcsb.org/rest/v1/core/entry/{id}`; root `pdb:{ID}` |
| Fetches | `GET https://data.rcsb.org/rest/v1/core/entry/{ID}` |
| Credentials | None |
| Rate | 1 request per second on `data.rcsb.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Dataset` |
| Keys | None (the entry's DOI is a fact only) |
| Credit | "RCSB PDB (CC0 1.0)", in `attribution` |
| Switch | `[sources.pdb] enabled`, on by default |

## Links

Every RCSB address of one entry is one resource. The root is `pdb:{ID}`, the four-character ID in
upper case, such as `pdb:4HHB`. These links are [aliases](../api.md#aliases) of it:

- `https://www.rcsb.org/structure/{id}` and the entry's other tabs: `/3d-view/{id}`,
  `/sequence/{id}`, `/annotations/{id}`, `/experimental/{id}` and `/versions/{id}`;
- the legacy `https://www.rcsb.org/pdb/explore.do?structureId={id}` and
  `https://www.rcsb.org/pdb/explore/explore.do?structureId={id}`;
- `https://data.rcsb.org/rest/v1/core/entry/{id}`.

An entry ID is a digit from 1 to 9 followed by three letters or digits. IDs are case-insensitive:
a link or root in another letter case, such as `pdb:4hhb`, is an alias of the upper-case root. The
extended form `pdb_0000{id}`, such as `pdb_00004hhb`, names the same entry, in a link and in a root.
A `pdb:` root that is not a valid ID is `missing` at once and never fetched.

Not read here: coordinate files on `files.rcsb.org` (files of an entry, not the entry), ligand
pages such as `rcsb.org/ligand/HEM`, deeper tab paths such as `/3d-view/{id}/1`, and other PDB
sites such as PDBe.

## What it fetches

One request per read: `GET https://data.rcsb.org/rest/v1/core/entry/{ID}`. An ID the API doesn't
know (404) is `missing`. An obsolete entry is `missing` too: the API answers it with 404, and the
entry that replaced it is not looked up. An answer for another entry is an error, and the read is
retried.

## Card

The type is `Dataset`.

- `site` is "RCSB PDB" and `name` the structure's title as deposited, often in capitals. The card
  has no description and no image.
- The `page_url` fact is the entry's page, `https://www.rcsb.org/structure/{ID}`. Because the root
  `pdb:{ID}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "RCSB PDB (CC0 1.0)".
- A structure is not an event, so its dates are facts, not `start`.

| Fact | Meaning |
| --- | --- |
| `doi` | The entry's own DOI in lower case (`10.2210/pdb{id}/pdb`), when the entry states it |
| `released`, `deposit_date`, `revision_date` | The initial release, the deposit and the last revision, `YYYY-MM-DD` |
| `methods` | The experimental methods, for example `X-RAY DIFFRACTION` |
| `resolution` | The resolution, in ångströms |
| `polymer_entity_count` | The number of distinct polymer entities |
| `molecular_weight_kda` | The deposited model's weight, in kDa |
| `classification` | The PDB classification, for example `OXYGEN TRANSPORT` |
| `keywords` | The entry's keywords, each once |
| `citation_title`, `citation_journal`, `citation_year` | The primary citation |
| `citation_doi`, `citation_pubmed_id` | The primary citation's DOI (in lower case) and PubMed ID |
| `page_url` | The entry's page on rcsb.org |

## Keys and groups

None. The entry's DOI is a fact only and joins no [group](../api.md#groups). The primary
citation's DOI and PubMed ID name another object, the paper, and are facts too.

## Credit

The `attribution` line credits RCSB PDB and states the data's licence, CC0 1.0.

## Turn it off

```toml
[sources.pdb]
enabled = false
```

RCSB links then get ordinary web cards, and `pdb:` roots wait until the source is on again.

## Examples

- `https://www.rcsb.org/structure/4HHB` (human deoxyhaemoglobin)
- `https://www.rcsb.org/structure/6VXX` (SARS-CoV-2 spike glycoprotein)
