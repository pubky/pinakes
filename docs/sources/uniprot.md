# UniProt

Pinakes reads protein entries in UniProtKB from the UniProt REST API, by accession. No key is
needed.

| | |
| --- | --- |
| Source | `uniprot`, official source |
| Links | `uniprot.org/uniprotkb/{acc}` and its tabs, the legacy `uniprot.org/uniprot/{acc}`, `rest.uniprot.org/uniprotkb/{acc}`, `purl.uniprot.org/uniprot/{acc}`, and entry-name links; root `uniprot:{ACC}` |
| Fetches | `GET https://rest.uniprot.org/uniprotkb/search?query=accession:{ACC}&fields=…&format=json&size=2`; first a search by name for an entry-name link |
| Credentials | None |
| Rate | 1 request per second on `rest.uniprot.org` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Protein` |
| Keys | None (facts only) |
| Credit | "UniProt (CC BY 4.0)", in `attribution` |
| Switch | `[sources.uniprot] enabled`, on by default |

## Links

Every UniProt address of one entry is one resource. The root is `uniprot:{ACC}`, the accession in
upper case, such as `uniprot:P69905`. These links are [aliases](../api.md#aliases) of it:

- `https://www.uniprot.org/uniprotkb/{acc}` and its tabs: `/entry`, `/feature-viewer`,
  `/variant-viewer`, `/publications`, `/external-links`, `/history` and `/genomic-coordinates`;
- the legacy `https://www.uniprot.org/uniprot/{acc}`;
- `https://rest.uniprot.org/uniprotkb/{acc}`;
- the `.txt`, `.xml` and `.json` forms of the page, legacy and REST addresses, such as
  `https://rest.uniprot.org/uniprotkb/{acc}.json`;
- `http://purl.uniprot.org/uniprot/{acc}`.

Accessions are case-insensitive: a link or root in another letter case, such as `uniprot:p69905`,
is an alias of the upper-case root. A `uniprot:` root that is not a valid accession (an entry name
among them) is `missing` at once and never fetched.

### Links by entry name

The same page, legacy and REST addresses can name an entry by its entry name instead of its
accession, such as `https://www.uniprot.org/uniprotkb/HBA_HUMAN/entry`. An entry name is up to 10
letters or digits, `_`, and up to 5 letters or digits. The PURL is read by accession only.

The link alone doesn't name the entry. Pinakes first searches for current entries with that name.
The link names the entry only when the search finds exactly one current entry with exactly that
name; a former name of a renamed entry is not enough. The link is then an alias of that entry's
root. Otherwise it is `missing`.

### Not read here

FASTA (`.fasta`) and GFF (`.gff`) files, which are the sequence or the features rather than the
entry; other tabs such as `/sequence-viewer`; searches; UniParc entries; and other sites, such as
the AlphaFold database.

## What it fetches

An accession is read through UniProt's search by accession, which asks only for the fields the
card uses (the full record is up to about 1 MB):

`GET https://rest.uniprot.org/uniprotkb/search?query=accession:{ACC}&fields=…&format=json&size=2`

The search never redirects. The entry endpoint would redirect a merged accession to the full record
of the entry it was merged into.

The answer decides the card:

- An empty answer, a deleted entry and an accession never assigned are `missing`.
- A merged accession answers with the one entry it was merged into. Pinakes reads that entry with
  a second search, and the link becomes an alias of its root. When that entry was merged again, the
  read fails.
- A demerged accession, or a merged one that names several entries, keeps a card of its own that
  names the accessions that replace it.
- An answer for other accessions, or for more than one entry, is an error, and the read is
  retried. So is any status other than 200.

An entry-name link first sends
`GET https://rest.uniprot.org/uniprotkb/search?query=id:{NAME} AND active:true&fields=accession,id&format=json&size=2`,
then reads the accession it finds as above.

## Card

The type is `Protein`.

- `site` is "UniProt" and `name` the recommended name, or else the first submitted name.
- `description` is UniProt's curated function text for the whole protein.
- The `page_url` fact is the entry's page, `https://www.uniprot.org/uniprotkb/{ACC}/entry`.
  Because the root `uniprot:{ACC}` is not a web link, the API also lists that page in `open_in`.
- `attribution` is "UniProt (CC BY 4.0)". The card has no image.
- The date the entry was first public is a fact (`first_public`), not a `start`.

The card of a demerged accession has no name, no description and only the facts `inactive`,
`replaced_by` and `page_url`.

| Fact | Meaning |
| --- | --- |
| `first_public` | The date the entry was first public, `YYYY-MM-DD` |
| `entry_name` | For example `HBA_HUMAN` |
| `gene`, `genes` | The gene name (`gene`), or the names when several genes encode the protein (`genes`) |
| `organism`, `taxon_id` | The organism's scientific name and NCBI taxonomy ID |
| `length` | The sequence length, in amino acids |
| `reviewed` | `true` for Swiss-Prot (reviewed), `false` for TrEMBL (unreviewed) |
| `sequence_version` | The sequence's version |
| `last_updated` | The date of the last annotation update, `YYYY-MM-DD` |
| `pdb_ids`, `pdb_count` | The first five PDB structures, and how many there are |
| `inactive`, `replaced_by` | For an inactive accession that keeps its own card: the reason (`demerged`, or `merged` into several entries) and the accessions that replace it |
| `page_url` | The entry's page on uniprot.org |

## Keys and groups

None. The card's IDs (PDB structures, the taxonomy ID) are facts only and join no
[group](../api.md#groups).

## Credit

The `attribution` line credits UniProt and states the data's licence, CC BY 4.0.

## Turn it off

```toml
[sources.uniprot]
enabled = false
```

UniProt links then get ordinary web cards, and `uniprot:` roots wait until the source is on again.

## Examples

- `https://www.uniprot.org/uniprotkb/P69905/entry` (haemoglobin subunit alpha)
- `https://www.uniprot.org/uniprotkb/P04637/entry` (p53)
