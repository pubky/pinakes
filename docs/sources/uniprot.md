# UniProt

Proteins in UniProtKB, read from the UniProt REST API by accession. CC BY 4.0, credited to
UniProt; no key; no published rate limit (one request per second).

## Links

`uniprot:{ACC}` and every UniProt address of an entry are one resource, with `uniprot:{ACC}` as
the root (the accession in upper case, e.g. `uniprot:P69905`):

- `https://www.uniprot.org/uniprotkb/{acc}` and its tabs: `/entry`, `/feature-viewer`,
  `/variant-viewer`, `/publications`, `/external-links`, `/history`, `/genomic-coordinates`;
- the legacy `https://www.uniprot.org/uniprot/{acc}`;
- `https://rest.uniprot.org/uniprotkb/{acc}`;
- `.txt`, `.xml` and `.json` of the site, legacy and REST addresses;
- `http://purl.uniprot.org/uniprot/{acc}`.

FASTA and GFF files are the sequence or the features, not the entry, and are not read.

The same addresses by entry name (`https://www.uniprot.org/uniprotkb/HBA_HUMAN/entry`) name the
entry once an exact lookup finds exactly one current entry with that name; a former name of a
renamed entry is not enough. They are aliases of that entry's root.

UniProt redirects a merged accession to the entry it was merged into: the link is then an alias of
that entry. A demerged accession keeps its own card, naming the accessions that replace it. A
deleted entry, an accession never assigned and an entry name with no current entry are `missing`.

## What it reads

`GET https://rest.uniprot.org/uniprotkb/{ACC}.json?fields=…` (the fields the card uses; the full
record can be close to 1 MB). An entry-name link first reads
`GET https://rest.uniprot.org/uniprotkb/search?query=id:{NAME} AND active:true&fields=accession,id&format=json&size=2`.

## Card

`Protein`, with `site` "UniProt", `name` the recommended name, `description` UniProt's curated
function text, `start` the date the entry was first public, and the attribution
"UniProt (CC BY 4.0)".

| Fact | Meaning |
| --- | --- |
| `entry_name` | E.g. `HBA_HUMAN` |
| `gene`, `genes` | The gene name, or the names when several genes encode the protein |
| `organism`, `taxon_id` | Scientific name and NCBI taxonomy id |
| `length` | Sequence length, in amino acids |
| `reviewed` | `true` for Swiss-Prot, `false` for TrEMBL |
| `sequence_version` | The sequence's version |
| `last_updated` | `YYYY-MM-DD` of the last annotation update |
| `pdb_ids`, `pdb_count` | The first five PDB structures, and how many there are |
| `inactive`, `replaced_by` | For a demerged accession: `demerged` and the accessions that replace it |
| `page_url` | The entry's page on uniprot.org |

The card also records the entry on AlphaFold DB when UniProt cross-references it.

## Examples

- `https://www.uniprot.org/uniprotkb/P69905/entry` (haemoglobin subunit alpha)
- `https://www.uniprot.org/uniprotkb/P04637/entry` (p53)
