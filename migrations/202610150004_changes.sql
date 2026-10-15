ALTER TABLE resource ADD COLUMN missing_streak integer NOT NULL DEFAULT 0;
ALTER TABLE resource ADD COLUMN last_missing_at timestamptz;
ALTER TABLE resource ADD COLUMN priority_at timestamptz;
ALTER TABLE resource ADD COLUMN osm_version text;
ALTER TABLE osm_place ADD COLUMN fetch_doc jsonb;
ALTER TABLE osm_place ADD COLUMN fetch_display_name text;
UPDATE osm_place SET fetch_display_name=display_name;
UPDATE osm_place SET fetch_doc=doc;
ALTER TABLE osm_place ADD COLUMN versions jsonb NOT NULL DEFAULT '{}';
UPDATE osm_place SET versions=jsonb_build_object(md5((doc-ARRAY['fetched_at','ttl_secs']::text[])::text),doc-ARRAY['fetched_at','ttl_secs']::text[]);
UPDATE resource r SET osm_version=md5((o.doc-ARRAY['fetched_at','ttl_secs']::text[])::text)
 FROM osm_place o WHERE r.osm_type=o.osm_type AND r.osm_id=o.osm_id AND r.resolved_at IS NOT NULL;

CREATE TABLE resource_change (
    seq bigserial PRIMARY KEY,
    resource_id text NOT NULL,
    occurrence_key text,
    at timestamptz NOT NULL DEFAULT now(),
    class text NOT NULL CHECK (class IN ('created','rescheduled','relocated','cancelled','uncancelled','renamed','details','deleted')),
    fields text[] NOT NULL,
    before jsonb,
    after jsonb,
    source text NOT NULL,
    type_path text[],
    osm_type text,
    osm_id bigint,
    CHECK (source<>'osm' OR (osm_type IS NOT NULL AND osm_id IS NOT NULL AND type_path IS NULL
      AND (before IS NULL OR before-ARRAY['osm_id','version']::text[]='{}'::jsonb)
      AND (after IS NULL OR after-ARRAY['osm_id','version']::text[]='{}'::jsonb)))
);
CREATE INDEX resource_change_resource_idx ON resource_change(resource_id,seq);
CREATE INDEX resource_change_at_idx ON resource_change(at);
CREATE INDEX resource_change_source_idx ON resource_change(source,seq);
CREATE INDEX resource_change_class_idx ON resource_change(class,seq);

CREATE TABLE source_state (
    source text PRIMARY KEY,
    cursor text,
    last_success_at timestamptz,
    last_error text,
    consecutive_failures integer NOT NULL DEFAULT 0,
    held_until timestamptz,
    keys text[] NOT NULL DEFAULT '{}',
    held_keys text[],
    missing_keys jsonb NOT NULL DEFAULT '{}',
    change_totals jsonb NOT NULL DEFAULT '{}',
    feed_held_total bigint NOT NULL DEFAULT 0,
    cursor_resets_total bigint NOT NULL DEFAULT 0,
    canary_at timestamptz,
    canary_ok boolean,
    canary_missing text[] NOT NULL DEFAULT '{}',
    canary_failures integer NOT NULL DEFAULT 0,
    canary_due_at timestamptz,
    canary_lease_until timestamptz,
    canary_generation bigint NOT NULL DEFAULT 0
);
ALTER TABLE feed_cursor ADD COLUMN resets bigint NOT NULL DEFAULT 0;

-- Historical OSM documents stay inside the derivative table, including after a resource is pruned.
CREATE VIEW change_document AS
 SELECT c.seq,c.resource_id,c.occurrence_key,c.at,c.class,c.fields,c.source,
   CASE WHEN c.source='osm' AND c.before IS NOT NULL THEN o.versions->(c.before->>'version') ELSE c.before END AS before,
   CASE WHEN c.source='osm' AND c.after IS NOT NULL THEN o.versions->(c.after->>'version') ELSE c.after END AS after,
   CASE WHEN c.source='osm' THEN '© OpenStreetMap contributors (ODbL)' END AS attribution,
   CASE WHEN c.source='osm' THEN coalesce(o.versions->(c.after->>'version')->'type_path',o.versions->(c.before->>'version')->'type_path') ELSE to_jsonb(c.type_path) END AS type_path
 FROM resource_change c LEFT JOIN osm_place o ON o.osm_type=c.osm_type AND o.osm_id=c.osm_id;

-- A cache refresh is not yet an accepted resource revision: serve its accepted OSM version.
CREATE OR REPLACE VIEW resource_document AS
 SELECT r.id,r.uri,r.scheme,r.host,r.type,r.type_path,r.name,r.category,
   r.point,r.start_at,r.end_at,r.venue_id,r.doc,r.search_text,
   r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r WHERE r.osm_id IS NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.type,o.type_path,o.name,o.category,
   o.point,r.start_at,r.end_at,r.venue_id,
   coalesce(o.versions->r.osm_version || jsonb_build_object('url',r.uri,'fetched_at',(extract(epoch FROM r.resolved_at)*1000)::bigint,'ttl_secs',greatest(0,extract(epoch FROM r.refresh_at-r.resolved_at)::bigint)), '{}'::jsonb),o.search_text,
   r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r LEFT JOIN osm_place o ON o.osm_type=r.osm_type AND o.osm_id=r.osm_id AND r.osm_version IS NOT NULL
 WHERE r.osm_id IS NOT NULL;
