# PubChem

Pinakes reads chemical compounds in PubChem from PUG REST, PubChem's REST API, by compound ID
(CID). No key is needed.

| | |
| --- | --- |
| Source | `pubchem`, official source |
| Links | `pubchem.ncbi.nlm.nih.gov/compound/{cid}`, the legacy `summary.cgi?cid={cid}`, PUG REST and PUG View URLs of a compound, and compound pages by name; root `pubchem:cid/{cid}` |
| Fetches | `GET https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/property/…/JSON`; first a CID lookup for a page by name |
| Credentials | None |
| Rate | At most 4 requests per second on `pubchem.ncbi.nlm.nih.gov` (PubChem asks for at most 5). This only lowers the shared per-host default (`limits.host_requests_per_second`, 1 by default) |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `MolecularEntity` |
| Keys | None (the InChIKey is a fact only) |
| Credit | "PubChem (NCBI)", in `attribution` |
| Switch | `[sources.pubchem] enabled`, on by default |

## Links

Every PubChem address of one compound is one resource. The root is `pubchem:cid/{cid}`, such as
`pubchem:cid/2244`. These links are [aliases](../api.md#aliases) of it:

- `https://pubchem.ncbi.nlm.nih.gov/compound/{cid}`;
- the legacy `https://pubchem.ncbi.nlm.nih.gov/summary/summary.cgi?cid={cid}`;
- the whole record or its properties in PUG REST:
  `https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/{format}` and
  `…/compound/cid/{cid}/property/{properties}/{format}`, where `{format}` is `JSON`, `XML`, `SDF`,
  `ASNT`, `ASNB`, `CSV` or `TXT`;
- the record in PUG View: `https://pubchem.ncbi.nlm.nih.gov/rest/pug_view/data/compound/{cid}`,
  and the same with `/JSON` or `/XML`.

A CID is up to 12 decimal digits without a leading zero. A `pubchem:` root that is not
`pubchem:cid/{cid}` with a valid CID is `missing` at once and never fetched.

### Pages by name

A compound page by name, such as `https://pubchem.ncbi.nlm.nih.gov/compound/aspirin`, is read
through a CID lookup. A name is at most 200 characters, without control characters.

- When PubChem lists exactly one CID for the name, the page gets that compound's card. It is never
  an alias of the compound: a name is not an identifier, so nothing merges on it. The page keeps a
  card of its own.
- When PubChem lists several CIDs, the page gets an ordinary web card.
- When PubChem lists none, the page is `missing`.

### Not read here

Other PUG REST operations name other things: the `/PNG` structure image, related compounds
(`/cids/…`), substances (`/sids/…`) and assays. Substance pages (`/substance/{sid}`) are
depositors' records, not compounds. PUG REST lookups by name (`/rest/pug/compound/name/…`) are not
read either.

## What it fetches

One request per read for a CID, asking for these properties (the card does not use
`ConnectivitySMILES`):

`GET https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/property/Title,IUPACName,MolecularFormula,MolecularWeight,InChIKey,SMILES,ConnectivitySMILES,XLogP,ExactMass/JSON`

A page by name first sends
`GET https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/{name}/cids/JSON`.

A CID PubChem doesn't know is `missing`: PUG REST answers it with a 404, or with a row that holds
only the CID (no InChIKey and no formula). An answer for another compound, or with other than one
row, is an error, and the read is retried.

## Card

The type is `MolecularEntity`.

- `site` is "PubChem" and `name` PubChem's title for the compound.
- `image` is the structure depiction,
  `https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/cid/{cid}/PNG`.
- `attribution` is "PubChem (NCBI)".
- There is no description.
- The `page_url` fact is the compound's page, `https://pubchem.ncbi.nlm.nih.gov/compound/{cid}`.
  Because the root `pubchem:cid/{cid}` is not a web link, the API also lists that page in
  `open_in`.

| Fact | Meaning |
| --- | --- |
| `inchikey` | The standard InChIKey |
| `iupac_name` | The IUPAC name |
| `formula` | The molecular formula |
| `molecular_weight` | The molecular weight, in g/mol |
| `exact_mass` | The monoisotopic mass |
| `smiles` | The full (isomeric) SMILES |
| `xlogp` | The computed partition coefficient (XLogP) |
| `page_url` | The compound's page on PubChem |

## Keys and groups

None. The InChIKey is a fact only and joins no [group](../api.md#groups).

## Credit

The `attribution` line credits PubChem at NCBI. PubChem states that its structure depictions (the
card's `image`) are public domain.

## Turn it off

```toml
[sources.pubchem]
enabled = false
```

PubChem links then get ordinary web cards, and `pubchem:` roots wait until the source is on again.

## Examples

- `https://pubchem.ncbi.nlm.nih.gov/compound/2244` (aspirin)
- `https://pubchem.ncbi.nlm.nih.gov/compound/2519` (caffeine)
