-- Let a selective OSM spatial scan find referencing resources without scanning the Pubky table.
CREATE INDEX resource_osm_idx ON resource(osm_type,osm_id) WHERE osm_id IS NOT NULL;
