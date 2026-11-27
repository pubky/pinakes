# `osm` catalogue: OpenStreetMap places

The OpenStreetMap catalogue imports named places and useful points from the planet file, or from
one region's extract, into `catalogue_card` (catalogue `osm`), and keeps them current with
OpenStreetMap's daily change files. Search lists them, a card nobody shared opens by its link,
and a shared openstreetmap.org link is read from its card instead of Nominatim.

| | |
| --- | --- |
| Catalogue | `osm` |
| Reads | `catalogues.osm.region = "planet"`: `https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf`, updates from `https://planet.openstreetmap.org/replication/day/`. A region (`europe/switzerland`): `https://download.geofabrik.de/<region>/<name>-latest.osm.pbf`, updates from `https://download.geofabrik.de/<region>/updates/` |
| Credentials | None. Downloads need `http.contact`, sent in the User-Agent: without it the importer waits, and `pinakesd catalogue status` says why |
| Stores | One card per kept object (below), with its place details; `osm_successor`, `osm_rename` and `osm_removed` from updates |
| Keys | `osm:<type>/<id>` and the card's own link's `url:` key |
| Canonical link | `https://www.openstreetmap.org/<type>/<id>` |
| Related link | The object's `website` (or `contact:website`, `url`; one naming a credential is left out), stored on the card and in the ODbL dump. A shared website joins the place's group (it takes the place's `osm:` key, which the place's shared link carries) when exactly one imported place names it; a chain's website, named by several branches, joins none. An OSM card still backs no link, and no resource row holds a value read from OpenStreetMap |
| Fame | The place's kind (`place=city` above `place=village`), its population, a `wikipedia` or `wikidata` tag; its Wikidata item's card's fame when that is higher. Unnamed points: 0 |
| Credit | `© OpenStreetMap contributors (ODbL)` on every card; the places are in the [ODbL dump](../operations.md#odbl-dump) |
| Updates | Looked for hourly; a new change file is applied when the server publishes one (daily). A full import every 30 days (the planet: every 90 days) |
| Switch | `[catalogues.osm] enabled`, on by default |

## What becomes a card

A node, way or relation is kept when it has a `name` and a category tag: `amenity`, `shop`,
`tourism`, `leisure`, `historic`, `natural`, `office`, `craft`, `place`, `public_transport`,
`healthcare` or `sport` (the first of these the object has, value not `yes` or `no`), or
`railway=station`, `halt` or `tram_stop`. Left out: transport stop positions
(`public_transport=stop_position`), signposts (`tourism=information` with
`information=guidepost` or `route_marker`), `place=farm`, `place=locality`,
`historic=boundary_stone`, parking entrances, and route and network relations.

These useful points are kept named or not, unless `access` is `private` or `no`: parking, ATMs,
toilets, charging stations, fuel, drinking water, pharmacies, banks, bike rental, car sharing,
recycling and post boxes (`amenity=parking`, `atm`, `toilets`, `charging_station`, `fuel`,
`drinking_water`, `pharmacy`, `bank`, `bicycle_rental`, `car_sharing`, `recycling`,
`post_box`). An unnamed one is named by its kind and its brand, else its operator (`ATM · UBS`),
else by its kind (`Parking`). Benches, bike parking and shelters stay out.

## Card

| Field | From |
| --- | --- |
| `type` | The category (`data/type-mappings.json`, `osm`): `amenity/restaurant` is `Restaurant`; else `Place` |
| `name`, `other_names` | `name`; `name:*`, `alt_name`, `official_name`, `short_name`, `old_name`, `int_name`, `loc_name`, `nat_name`, `reg_name` (`;` separates several) |
| `description` | The category's value and the city: `Restaurant in Vaduz` |
| `labels` | The category's value, `cuisine`, `sport`, `religion`, `denomination`, and `vegan`/`vegetarian` from `diet:*` |
| `point` | A node's location; a way's mean node location; a relation's `label` or `admin_centre` node, else the centre of its members' box |
| `details` | `category`, `address` (street, house number, postcode, city, country code), `tags` (opening hours, website, phone, email, cuisine, wheelchair, brand, operator, Wikidata and Wikipedia tags and the other tags the OSM route keeps), `bbox` for a way or relation, `unnamed` for a point named by its kind |

A card nobody shared opens by its link (`GET /v1/resources/by-uri`, any spelling of an
openstreetmap.org object link) and by its link's ID (`GET /v1/resources/{id}`) as an OSM link's
card does: the same fields, built from the catalogue card; nothing is stored. A shared OSM link
(`POST /v1/resolve`, a reference) of an imported object is read from its card into `osm_place`,
with no Nominatim request; an object the catalogue does not hold is still read from Nominatim.
When an update changes or removes a card, its shared links are read again at once.

## Import and updates

A full import downloads the file into `catalogues.dir/osm`, then reads it twice. The first pass
writes every node's location into a node index on disk and writes the node and way cards; the
second reads the kept relations' member ways for their boxes and writes the relation cards. The
planet's index (and that of a file over 40 GB) takes 8 bytes per node ID (about 115 GB at today's
IDs, a sparse file); a region's 16 bytes per node, about twice its file.

The planet's file and change files come from `planet.openstreetmap.org`, which redirects them to
OpenStreetMap's buckets (`osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com`,
`osm-planet-us-west-2.s3.dualstack.us-west-2.amazonaws.com`); a region's from
`download.geofabrik.de`. The importer follows at most 5 redirects, only to these names, over https.

- Before the download, the importer checks the free disk for the file and its index (the file's
  length is known from the server), and waits while it is short (`waiting`), as before any run.
- A download broken within one attempt resumes where it broke, only from the same file (its
  `ETag`, sent as `If-Range`); a new attempt (a restart) downloads it again from its start. A
  finished download is used again by a later attempt for 7 days, so a failed import reads the same
  file again without downloading it; a changed region downloads its own file.
- An index taking more disk than its file can need (the dense one: half again its size today,
  about 172 GB), or leaving under 1 GB free, stops the import (`disk`).
- A run cut off resumes from its last written part, on the same file: `catalogue import osm
  --from` another file (or the same name holding another file) while one is under way is refused
  (`other_file`). A run whose file changed starts over in a new generation, so the cards it wrote
  from the first file go at its end: a changed region, the file gone, or the file downloaded anew
  (its earlier download expired, or another file's header). A run cut off after its last part
  finishes without reading the file again, unless it was another region's: that one starts over.
- At its end, the import removes the cards the file no longer holds, a part at a time, and marks
  their shared links due once they are gone. A named place swept so that someone shared keeps
  its last card in `osm_removed`, for the deleted-place rule.
- The shrink floor applies first, as to any catalogue's full import: a re-import that read under
  half the cards held (a cut-off file, a smaller region) removes nothing and fails `shrunk`, its
  run kept, until `pinakesd catalogue allow-shrink osm` lets the next try through. The file and
  the node index are removed first: the run's end needs neither, so a run waiting there holds no
  disk.
- A file that holds no place is refused (`empty_file`): it would empty the catalogue. The next
  attempt starts over on another file (a new download, or another `--from` file).
- A change file must end with its `</osmChange>`: one cut off between two actions is refused
  (`parse`), so an update never moves past changes it did not read.
- The file and the index are removed once the import has finished.
- A full import after the first marks due the shared links of each card whose content it changes
  (an unchanged card would read the same). It looks for no successors (only an update sees an
  object deleted) and records no renames (it compares no names).
- A way, or a relation of members either side of the antimeridian, gets its point and box on it
  (a box whose west edge is east of its east edge), not one spanning the world.
- Characters that hide in text or turn its direction (the zero-width space, direction marks,
  embeddings, overrides, isolates, the word joiner, a byte order mark) are taken out of every tag
  value as the file is read; a value with a control character is left out. The zero-width
  non-joiner and joiner stay (Persian, Indic scripts).

The import's replication sequence and time (from the file's header) are where the updates start.
A header's sequence is used only when it names this region's replication stream; else the updates
start at the last change file published before the file's time.

An update reads the change files after that sequence, at most 30 a run. Each file is read to
`catalogues.dir/osm` first (at most 4 GB, with room for twice its length), and an object it holds
several edits of is read at its last one (a delete wins a tie). A file whose changes would hold
more than 1 GiB in memory is refused (`too_large`). The update:

- writes the cards of created and modified objects the filter keeps, and removes deleted ones and
  those that no longer pass the filter. A way's geometry comes from the nodes the file holds; when
  it lacks some, the way keeps its stored geometry. A way whose nodes moved without the way itself
  changing keeps its geometry, and a way newly kept without its nodes in the file has none, until
  the next full import (every 30 days, the planet's every 90);
- records a deleted object's successor (`osm_successor`) on evidence only: the one created or
  stored object tagged with the same Wikidata item within 1 km, or the one object created in the
  same changeset with the same name within 50 m. Geofabrik's change files name no changesets, so a
  region's updates find successors by Wikidata only. Shared links of the deleted object become
  aliases of the successor's link (also a link first read after the successor was found, once it
  reads as missing). When the deleted object comes back (the deletion is reverted), its successor
  is forgotten and its links answer for themselves again;
- records a named deleted object's last card (`osm_removed`: its name, category and point), which
  the deleted-place rule compares once the card is gone; gone again when the object comes back;
- records a kept object's change to an unrelated name (`osm_rename`): a name whose words don't run
  whole within the other's ("Engel" and "Restaurant Engel" are one place; "Ox" and "Fox &
  Hounds" are not) and that differs by more than a fifth of its letters (two at least). Spelling
  fixes and small renames stay the same place.

A change file missing just behind the server's newest is tried again at the next update (a mirror
catching up); one missing 3 or more behind it is gone. An update that finds the region changed, a
change file gone, the server's sequence behind the
stored one, or the files more than 60 behind asks for a full import. Its successors and renames are
recorded before its cards are written, so an update cut off and applied again keeps them.

## Place rules

With the catalogue on, a resource read (`GET /v1/resources/{id}`, `GET /v1/resources/by-uri`)
applies these rules ([API](../api.md#places)):

- A map link whose name and point come from the link itself (Google Maps, Apple Maps, Organic
  Maps, `geo:`; an OsmAnd link names no place, so it stays a point) and the one imported place with the same name within 50 m list each
  other as maybe the same, in `maybe_same` and in `also_on` (`kind: "maybe"`; on the place, with
  the link's `refs`, so its tags show with the place's). With no such place, or several, the link
  stays a point. Names are compared without case, accents, spaces and punctuation. Nothing is
  merged or grouped.
- A deleted object's link lists the one imported place of its name and category within 50 m of
  where it was in `maybe_same`: its name, category and point as its card last was
  (`osm_removed`), else as its link was last read. Another category is never used.
- A place renamed to an unrelated name lists the places it was in `replaced`, each with when it
  was replaced and how many references the link got before.

Pinakes never rewrites a link and never edits OpenStreetMap.

## Run it yourself

`pinakesd catalogue import osm [--from FILE]` runs one full import now, as the daemon's task
runs it (the large-import turn, the disk check, the cursor), from a PBF file or from the
configured region; an import cut off is taken up again (on its own file). `pinakesd catalogue
import osm --update` runs one update from the region's change files, after a finished import.
Both refuse while another process runs the importer ([operations](../operations.md#importer-rules)).

`scripts/osm-catalogue-sample` imports a real extract a few days old (Liechtenstein by default)
into a new throwaway database, runs the update (the region's real change files since), and checks
the cards, search, the reads, the map clusters, the ODbL dump and the off switch; it prints its
measurements as JSON. It needs `DATABASE_URL` (a server it may create a database on),
`PINAKES__HTTP__CONTACT` (or `~/.config/pinakes/contact.env`) and a built `pinakesd`;
`--region`, `--days`, `--max-mb` (300 by default) and `--keep` change what it reads and keeps.
Measured (debug build, a busy host): Liechtenstein, 2,456 cards in 4.5 s; Switzerland, 400,838
cards in 596 s, about 1.4 KB per card in PostgreSQL.

## Credit

Every card, and every OSM link's card read from one, carries `attribution`
`© OpenStreetMap contributors (ODbL)`. The imported places are in the
[ODbL dump](../api.md#osm-dump).

## Examples

- `GET /v1/resources/by-uri?uri=https://www.openstreetmap.org/node/240079149`: Schaan, an
  imported town, by its link.
- `GET /v1/search?q=schaan&type=City`: the same town in search.
- `GET /v1/map/clusters?bbox=9.47,47.04,9.64,47.27&zoom=11`: Liechtenstein's places per cell.

## Turn it off

`[catalogues.osm] enabled = false`: no import runs, the cards leave search at once, OSM links are
read from Nominatim again, the place rules and the dump's imported places go. The cards stay until
`pinakesd catalogue drop osm`, which also forgets the successors, renames and removed objects
updates recorded, releasing the aliases the successors made.
