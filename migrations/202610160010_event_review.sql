-- Container bookkeeping belongs to the referenced feed, not a Prometheus source label.
ALTER TABLE event_fetch ADD COLUMN keys text[] NOT NULL DEFAULT '{}';
ALTER TABLE event_fetch ADD COLUMN held_keys text[];
ALTER TABLE event_fetch ADD COLUMN missing_keys jsonb NOT NULL DEFAULT '{}';
ALTER TABLE event_fetch ADD COLUMN hold_count integer NOT NULL DEFAULT 0;
ALTER TABLE event_fetch ADD COLUMN retention_days integer NOT NULL DEFAULT 30 CHECK(retention_days BETWEEN 1 AND 3650);
ALTER TABLE event_fetch ADD COLUMN horizon_start timestamptz;
UPDATE event_fetch e SET keys=s.keys,held_keys=s.held_keys,missing_keys=s.missing_keys
 FROM source_state s WHERE s.source='calendar:'||e.resource_id;
INSERT INTO source_state(source,feed_held_total) SELECT 'event',coalesce(sum(feed_held_total),0) FROM source_state WHERE source LIKE 'calendar:%'
 ON CONFLICT(source) DO UPDATE SET feed_held_total=source_state.feed_held_total+excluded.feed_held_total;
DELETE FROM source_state WHERE source LIKE 'calendar:%';

CREATE FUNCTION event_boundary(d jsonb) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT coalesce(d#>>'{venue,location_source}' NOT IN ('geocoded','osm'),true)
 OR (NOT d ? 'location' AND NOT (d->'venue') ? 'location')
$$;
ALTER TABLE resource ADD CONSTRAINT resource_event_boundary CHECK(event_boundary(doc) AND (coalesce(doc#>>'{venue,location_source}' NOT IN ('geocoded','osm'),true) OR point IS NULL));
ALTER TABLE occurrence ADD CONSTRAINT occurrence_event_boundary CHECK(event_boundary(doc) AND (coalesce(doc#>>'{venue,location_source}' NOT IN ('geocoded','osm'),true) OR point IS NULL));
ALTER TABLE resource_change ADD CONSTRAINT change_event_boundary CHECK(event_boundary(before) AND event_boundary(after));
ALTER TABLE reference ADD CONSTRAINT reference_event_status CHECK(event_status IS NULL OR event_status IN ('scheduled','cancelled','postponed','rescheduled','moved_online'));
ALTER TABLE resource ADD CONSTRAINT resource_event_status CHECK(doc->>'event_status' IS NULL OR doc->>'event_status' IN ('scheduled','cancelled','postponed','rescheduled','moved_online'));
ALTER TABLE occurrence ADD CONSTRAINT occurrence_status CHECK(status IN ('scheduled','cancelled','postponed','rescheduled','moved_online','superseded'));
ALTER TABLE occurrence ADD COLUMN search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple',coalesce(name,'')||' '||coalesce(doc->>'description',''))) STORED;
CREATE INDEX occurrence_search_idx ON occurrence USING gin(search_text);
CREATE INDEX occurrence_geography_idx ON occurrence USING gist((point::geography));
CREATE INDEX occurrence_page_idx ON occurrence(resource_id,start_at,uid,recurrence_id);
CREATE INDEX occurrence_hash_idx ON occurrence(resource_id,content_hash,start_at,name);
CREATE INDEX occurrence_retention_idx ON occurrence(end_at,start_at);

CREATE OR REPLACE FUNCTION event_document(d jsonb) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT CASE WHEN d#>>'{venue,osm_id}' IS NULL THEN d ELSE coalesce((
 SELECT CASE WHEN p IS NULL THEN d ELSE d || jsonb_build_object('location',p->'location','location_source',d#>'{venue,location_source}','attribution','© OpenStreetMap contributors (ODbL)','venue',(d->'venue')||jsonb_build_object('location',p->'location')) END
 FROM osm_place o CROSS JOIN LATERAL (SELECT o.versions->(d#>>'{venue,osm_version}') AS p) v
 WHERE o.osm_type=split_part(d#>>'{venue,osm_id}','/',1)
 AND o.osm_id=CASE WHEN split_part(d#>>'{venue,osm_id}','/',2) ~ '^[0-9]{1,18}$' THEN split_part(d#>>'{venue,osm_id}','/',2)::bigint END
 ),d) END
$$;
-- Expose indexed geometry directly in every arm. Ordinary rows never call event_document.
CREATE OR REPLACE VIEW resource_document AS
 SELECT r.id,r.uri,r.scheme,r.host,r.type,r.type_path,r.name,r.category,r.point,r.start_at,r.end_at,r.venue_id,r.doc,r.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r WHERE r.osm_id IS NULL AND r.doc#>>'{venue,osm_id}' IS NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,r.type,r.type_path,r.name,r.category,o.point,r.start_at,r.end_at,r.venue_id,event_document(r.doc),r.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r LEFT JOIN osm_place o ON o.osm_type=split_part(r.doc#>>'{venue,osm_id}','/',1)
 AND o.osm_id=CASE WHEN split_part(r.doc#>>'{venue,osm_id}','/',2) ~ '^[0-9]{1,18}$' THEN split_part(r.doc#>>'{venue,osm_id}','/',2)::bigint END
 WHERE r.osm_id IS NULL AND r.doc#>>'{venue,osm_id}' IS NOT NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.type,o.type_path,o.name,o.category,o.point,r.start_at,r.end_at,r.venue_id,
 coalesce(o.versions->r.osm_version || jsonb_build_object('url',r.uri,'fetched_at',(extract(epoch FROM r.resolved_at)*1000)::bigint,'ttl_secs',greatest(0,extract(epoch FROM r.refresh_at-r.resolved_at)::bigint)), '{}'::jsonb),o.search_text,
 r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r LEFT JOIN osm_place o ON o.osm_type=r.osm_type AND o.osm_id=r.osm_id AND r.osm_version IS NOT NULL WHERE r.osm_id IS NOT NULL;
DROP VIEW search_unit;
DROP VIEW event_occurrence;
CREATE VIEW event_occurrence AS
 SELECT o.*,o.doc AS document,o.point AS effective_point FROM occurrence o WHERE o.doc#>>'{venue,osm_id}' IS NULL AND o.status<>'superseded'
 UNION ALL
 SELECT x.*,event_document(x.doc) AS document,o.point AS effective_point FROM occurrence x LEFT JOIN osm_place o
 ON o.osm_type=split_part(x.doc#>>'{venue,osm_id}','/',1)
 AND o.osm_id=CASE WHEN split_part(x.doc#>>'{venue,osm_id}','/',2) ~ '^[0-9]{1,18}$' THEN split_part(x.doc#>>'{venue,osm_id}','/',2)::bigint END
 WHERE x.doc#>>'{venue,osm_id}' IS NOT NULL AND x.status<>'superseded';
CREATE VIEW search_unit AS
 SELECT r.*,r.id AS unit_id,NULL::jsonb AS occurrence,NULL::jsonb AS event,NULL::text AS event_author
 FROM resource_document r WHERE NOT coalesce(r.type_path @> ARRAY['Event'],false) OR r.start_at IS NOT NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.doc->>'type',ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')),o.name,NULL::text,
 o.effective_point,o.start_at,o.end_at,o.venue_id,o.document,o.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,NULL::text,NULL::bigint,
 md5(jsonb_build_array(r.id,o.uid,o.recurrence_id)::text),jsonb_build_object('uid',o.uid,'recurrence_id',nullif(o.recurrence_id,''),'event_resource_id',o.event_resource_id),NULL::jsonb,NULL::text
 FROM event_occurrence o JOIN resource r ON r.id=o.resource_id
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,'Event',ARRAY['Thing','Event'],NULL::text,NULL::text,r.point,f.start_at,f.end_at,r.id,
 jsonb_build_object('type','Event','type_path',jsonb_build_array('Thing','Event'),'url',r.uri,'site',coalesce(r.host,''),'source','nexus_reference','status','ok','attributes','{}'::jsonb,'start',f.start_at,'end',f.end_at,'event_status',coalesce(f.event_status,'scheduled')) || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',r.doc->'location') END,
 ''::tsvector,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id,
 md5(jsonb_build_array(r.id,f.author,f.object_path,f.recurrence_id)::text),NULL::jsonb,jsonb_build_object('author',f.author,'object_path',f.object_path,'recurrence_id',f.recurrence_id,'start',f.start_at,'end',f.end_at,'status',coalesce(f.event_status,'scheduled')),f.author
 FROM reference f JOIN resource_document r ON r.id=f.resource_id WHERE f.kind='event' AND f.start_at IS NOT NULL;
