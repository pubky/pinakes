CREATE EXTENSION IF NOT EXISTS h3;

CREATE TABLE osm_place (
    osm_type text NOT NULL CHECK (osm_type IN ('node','way','relation')),
    osm_id bigint NOT NULL CHECK (osm_id > 0),
    point geometry(Point,4326),
    bbox double precision[],
    name text,
    category text,
    address jsonb NOT NULL DEFAULT '{}',
    tags jsonb NOT NULL DEFAULT '{}',
    display_name text,
    type text NOT NULL,
    type_path text[] NOT NULL,
    doc jsonb NOT NULL,
    fetched_at timestamptz NOT NULL,
    refresh_at timestamptz NOT NULL,
    status text NOT NULL CHECK (status IN ('ok','missing')),
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(display_name, ''))) STORED,
    PRIMARY KEY(osm_type, osm_id),
    CHECK (octet_length(doc::text) <= 8192)
);
COMMENT ON TABLE osm_place IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';
CREATE INDEX osm_place_point_idx ON osm_place USING gist(point);
CREATE INDEX osm_place_geography_idx ON osm_place USING gist((point::geography));
CREATE INDEX osm_place_category_idx ON osm_place(category);
CREATE INDEX osm_place_type_path_idx ON osm_place USING gin(type_path);
CREATE INDEX osm_place_search_idx ON osm_place USING gin(search_text);
CREATE INDEX osm_place_name_idx ON osm_place USING gin(name gin_trgm_ops);
CREATE INDEX resource_geography_idx ON resource USING gist((point::geography));
ALTER TABLE resource ADD COLUMN osm_type text;
ALTER TABLE resource ADD COLUMN osm_id bigint;
ALTER TABLE resource ADD COLUMN final boolean NOT NULL DEFAULT false;
ALTER TABLE resource ALTER COLUMN type_path DROP NOT NULL;
ALTER TABLE resource ADD CONSTRAINT resource_osm_separation CHECK (
    (osm_id IS NULL AND osm_type IS NULL) OR
    (osm_id > 0 AND osm_type IN ('node','way','relation') AND resolver='osm'
      AND name IS NULL AND point IS NULL AND category IS NULL AND type IS NULL AND type_path IS NULL
      AND doc - ARRAY['type','url','osm_id','source','status']::text[] = '{}'::jsonb)
);
UPDATE resource SET resolver='geo' WHERE scheme='geo';
UPDATE resource SET resolver='web' WHERE resolver IS NULL AND scheme IN ('http','https');
UPDATE resource SET resolver='osm', osm_type=split_part(uri,'/',4), osm_id=split_part(uri,'/',5)::bigint, type_path=NULL
 WHERE uri ~ '^https://www[.]openstreetmap[.]org/(node|way|relation)/[1-9][0-9]{0,17}$';

-- A view, not a materialized copy: predicates push into each independently indexed arm.
CREATE VIEW resource_document AS
 SELECT r.id, r.uri, r.scheme, r.host, r.type, r.type_path, r.name, r.category,
        r.point, r.start_at, r.end_at, r.venue_id, r.doc, r.search_text,
        r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id
 FROM resource r WHERE r.osm_id IS NULL
 UNION ALL
 SELECT r.id, r.uri, r.scheme, r.host, o.type, o.type_path, o.name, o.category,
        o.point, r.start_at, r.end_at, r.venue_id,
        coalesce(o.doc || jsonb_build_object('url', r.uri), '{}'::jsonb), o.search_text,
        r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id
 FROM resource r LEFT JOIN osm_place o ON o.osm_type=r.osm_type AND o.osm_id=r.osm_id
 WHERE r.osm_id IS NOT NULL;
