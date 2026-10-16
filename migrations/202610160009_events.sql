-- Keep every located post occurrence (and each venue) without changing URI identities.
UPDATE resource SET resolver='event',final=false,dead_letter=false,attempts=0,next_attempt_at=now(),lease_until=NULL,generation=generation+1
 WHERE (scheme IN ('webcal','webcals') OR (scheme IN ('http','https') AND uri ~ '(\.ics|/ical/|/feed/(ics|rss|json)|/public-calendars/[^?#]*)([?#].*)?$')) AND resolver IS DISTINCT FROM 'osm';
ALTER TABLE reference ADD COLUMN unit_key text NOT NULL DEFAULT '';
UPDATE reference SET unit_key=resource_id||':'||coalesce(recurrence_id,'') WHERE kind='event';
ALTER TABLE reference DROP CONSTRAINT reference_pkey;
ALTER TABLE reference ADD PRIMARY KEY(author,object_path,unit_key);
ALTER TABLE reference ADD COLUMN event_status text;
CREATE INDEX reference_event_idx ON reference(resource_id,start_at) WHERE kind='event';

ALTER TABLE occurrence ADD COLUMN doc jsonb NOT NULL DEFAULT '{}';
ALTER TABLE occurrence ADD COLUMN point geometry(Point,4326);
ALTER TABLE occurrence ADD COLUMN sequence bigint NOT NULL DEFAULT 0;
ALTER TABLE occurrence ADD COLUMN modified timestamptz;
ALTER TABLE occurrence ADD COLUMN content_hash text NOT NULL DEFAULT '';
ALTER TABLE occurrence ADD COLUMN volatile boolean NOT NULL DEFAULT false;
ALTER TABLE occurrence ADD COLUMN supersedes text;
ALTER TABLE occurrence ADD CONSTRAINT occurrence_document_cap CHECK(octet_length(doc::text)<=8192);
CREATE INDEX occurrence_point_idx ON occurrence USING gist(point);
CREATE TRIGGER occurrence_revision BEFORE INSERT OR UPDATE OR DELETE ON occurrence FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TABLE event_fetch(resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,validators jsonb NOT NULL DEFAULT '{}',last_snapshot_at bigint);

-- Geocoded facts live only in the ODbL table; event records hold a versioned reference.
CREATE FUNCTION event_document(d jsonb) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT CASE WHEN p IS NULL THEN d ELSE d || jsonb_build_object('location',p->'location','location_source',d#>'{venue,location_source}','attribution','© OpenStreetMap contributors (ODbL)','venue',(d->'venue')||jsonb_build_object('location',p->'location')) END
 FROM (SELECT (SELECT o.versions->(d#>>'{venue,osm_version}') FROM osm_place o WHERE o.osm_type||'/'||o.osm_id=d#>>'{venue,osm_id}') AS p) v
$$;
CREATE OR REPLACE VIEW resource_document AS
 SELECT r.id,r.uri,r.scheme,r.host,r.type,r.type_path,r.name,r.category,
   coalesce(r.point,CASE WHEN d.doc->'location' IS NOT NULL THEN ST_SetSRID(ST_MakePoint((d.doc#>>'{location,lon}')::float8,(d.doc#>>'{location,lat}')::float8),4326) END)::geometry(Point,4326) AS point,
   r.start_at,r.end_at,r.venue_id,d.doc,r.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r CROSS JOIN LATERAL (SELECT event_document(r.doc) AS doc) d WHERE r.osm_id IS NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.type,o.type_path,o.name,o.category,o.point,r.start_at,r.end_at,r.venue_id,
   coalesce(o.versions->r.osm_version || jsonb_build_object('url',r.uri,'fetched_at',(extract(epoch FROM r.resolved_at)*1000)::bigint,'ttl_secs',greatest(0,extract(epoch FROM r.refresh_at-r.resolved_at)::bigint)), '{}'::jsonb),o.search_text,
   r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id
 FROM resource r LEFT JOIN osm_place o ON o.osm_type=r.osm_type AND o.osm_id=r.osm_id AND r.osm_version IS NOT NULL WHERE r.osm_id IS NOT NULL;

CREATE VIEW event_occurrence AS
 SELECT o.*,event_document(o.doc) AS document,
 coalesce(o.point,CASE WHEN d.doc->'location' IS NOT NULL THEN ST_SetSRID(ST_MakePoint((d.doc#>>'{location,lon}')::float8,(d.doc#>>'{location,lat}')::float8),4326) END) AS effective_point
 FROM occurrence o CROSS JOIN LATERAL (SELECT event_document(o.doc) AS doc) d;

-- Every branch has one score owner but a distinct key for pagination/clustering.
CREATE VIEW search_unit AS
 SELECT r.*,r.id AS unit_id,NULL::jsonb AS occurrence,NULL::jsonb AS event,NULL::text AS event_author
 FROM resource_document r WHERE NOT coalesce(r.type_path @> ARRAY['Event'],false) OR r.start_at IS NOT NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.doc->>'type',ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')),o.name,NULL::text,
 o.effective_point,o.start_at,o.end_at,o.venue_id,o.document,
 to_tsvector('simple',coalesce(o.name,'')||' '||coalesce(o.doc->>'description','')),r.resolver,r.resolved_at,r.refresh_at,r.status,NULL::text,NULL::bigint,
 md5(jsonb_build_array(r.id,o.uid,o.recurrence_id)::text),jsonb_build_object('uid',o.uid,'recurrence_id',nullif(o.recurrence_id,''),'event_resource_id',coalesce(o.event_resource_id,linked.id)),NULL::jsonb,NULL::text
 FROM event_occurrence o JOIN resource_document r ON r.id=o.resource_id LEFT JOIN resource linked ON linked.uri=o.url
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,'Event',ARRAY['Thing','Event'],NULL::text,NULL::text,r.point,f.start_at,f.end_at,r.id,
 jsonb_build_object('type','Event','type_path',jsonb_build_array('Thing','Event'),'url',r.uri,'site',coalesce(r.host,''),'source','nexus_reference','status','ok','attributes','{}'::jsonb,'start',f.start_at,'end',f.end_at,'event_status',coalesce(f.event_status,'scheduled')) || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',r.doc->'location') END,
 ''::tsvector,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id,
 md5(jsonb_build_array(r.id,f.author,f.object_path,f.recurrence_id)::text),NULL::jsonb,jsonb_build_object('author',f.author,'object_path',f.object_path,'recurrence_id',f.recurrence_id,'start',f.start_at,'end',f.end_at,'status',coalesce(f.event_status,'scheduled')),f.author
 FROM reference f JOIN resource_document r ON r.id=f.resource_id WHERE f.kind='event' AND f.start_at IS NOT NULL;

CREATE OR REPLACE VIEW change_document AS
 SELECT c.seq,c.resource_id,c.occurrence_key,c.at,c.class,c.fields,c.source,
   CASE WHEN c.source='osm' AND c.before IS NOT NULL THEN o.versions->(c.before->>'version') ELSE event_document(c.before) END AS before,
   CASE WHEN c.source='osm' AND c.after IS NOT NULL THEN o.versions->(c.after->>'version') ELSE event_document(c.after) END AS after,
   CASE WHEN c.source='osm' OR c.before#>>'{venue,osm_id}' IS NOT NULL OR c.after#>>'{venue,osm_id}' IS NOT NULL THEN '© OpenStreetMap contributors (ODbL)' END AS attribution,
   CASE WHEN c.source='osm' THEN coalesce(o.versions->(c.after->>'version')->'type_path',o.versions->(c.before->>'version')->'type_path') ELSE to_jsonb(c.type_path) END AS type_path
 FROM resource_change c LEFT JOIN osm_place o ON o.osm_type=c.osm_type AND o.osm_id=c.osm_id;

CREATE INDEX resource_venue_version_idx ON resource((doc#>>'{venue,osm_id}'),(doc#>>'{venue,osm_version}'));
CREATE INDEX occurrence_venue_version_idx ON occurrence((doc#>>'{venue,osm_id}'),(doc#>>'{venue,osm_version}'));
CREATE INDEX resource_change_before_venue_idx ON resource_change((before#>>'{venue,osm_id}'),(before#>>'{venue,osm_version}'));
CREATE INDEX resource_change_after_venue_idx ON resource_change((after#>>'{venue,osm_id}'),(after#>>'{venue,osm_version}'));
