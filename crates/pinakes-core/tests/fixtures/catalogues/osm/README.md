# OpenStreetMap catalogue fixtures

Real OpenStreetMap data, © OpenStreetMap contributors, available under the Open Database
License (ODbL 1.0, https://opendatacommons.org/licenses/odbl/1-0/). Geofabrik's public extracts
carry no user names, user IDs or changeset IDs.

| File | Source | Read |
| --- | --- | --- |
| `liechtenstein-part.osm.pbf` | Five of the 53 blobs of `https://download.geofabrik.de/europe/liechtenstein/liechtenstein-latest.osm.pbf` (Geofabrik file `liechtenstein-261124.osm.pbf`, 3,527,490 bytes): the header, the first node blob, the node blob before the first way blob, the first way blob and the first relation blob, copied byte for byte | 2026-11-26 |
| `liechtenstein-4978.osc.gz` | `https://download.geofabrik.de/europe/liechtenstein/updates/000/004/978.osc.gz`, the region's daily change file 4978, unchanged | 2026-11-26 |

The part's ways and relations name nodes from blobs left out, so some of their geometry is
missing: the tests read it as the importer reads a damaged file's part.
