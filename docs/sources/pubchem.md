# PubChem

Chemical compounds in PubChem, read from PUG REST by compound id (CID). Public data, credited to
PubChem; no key. PubChem asks for at most 5 requests a second (this source uses 4) and reports its
load in the `X-Throttling-Control` header.

## Links

`pubchem:cid/{cid}` and every PubChem address of a compound are one resource, with
`pubchem:cid/{cid}` as the root (e.g. `pubchem:cid/2244`):

- `https://pubchem.ncbi.nlm.nih.gov/compound/{cid}`;
- the legacy `https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?cid={cid}`;
- `https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/…` and
  `https://pubchem.ncbi.nlm.nih.gov/rest/pug_view/data/compound/{cid}/…`.

A page by name (`https://pubchem.ncbi.nlm.nih.gov/compound/aspirin`) names the compound when
PubChem lists exactly one CID for the name; it is then an alias of that compound's root. Substances
(`/substance/{sid}`) are depositors' records, not compounds, and are not read.

A CID PubChem does not know, and a name it lists no CID for (or several), is `missing`.

## What it reads

`GET https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/property/Title,IUPACName,MolecularFormula,MolecularWeight,InChIKey,SMILES,ConnectivitySMILES,XLogP,ExactMass/JSON`.
A name link first reads `GET https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/{name}/cids/JSON`.

## Card

`MolecularEntity`, with `site` "PubChem", `name` PubChem's title for the compound, `image` the
structure depiction (`…/compound/cid/{cid}/PNG`, public domain) and the attribution
"PubChem (NCBI)". There is no description: PubChem's descriptions are texts of other databases,
each under its own licence.

| Fact | Meaning |
| --- | --- |
| `inchikey` | The standard InChIKey |
| `iupac_name` | The IUPAC name |
| `formula` | The molecular formula |
| `molecular_weight` | In g/mol |
| `exact_mass` | The monoisotopic mass |
| `smiles` | The full (isomeric) SMILES |
| `xlogp` | The computed XLogP3 |
| `page_url` | The compound's page on PubChem |

## Examples

- `https://pubchem.ncbi.nlm.nih.gov/compound/2244` (aspirin)
- `https://pubchem.ncbi.nlm.nih.gov/compound/2519` (caffeine)
