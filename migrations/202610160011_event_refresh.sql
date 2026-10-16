-- NULL on upgrade forces a fresh body before trusting validators.
ALTER TABLE event_fetch ADD COLUMN horizon_end timestamptz;

-- Retain history for direct reads, but hide units whose feed is confirmed missing.
CREATE OR REPLACE VIEW search_unit AS
 SELECT r.*,r.id AS unit_id,NULL::jsonb AS occurrence,NULL::jsonb AS event,NULL::text AS event_author
 FROM resource_document r WHERE NOT coalesce(r.type_path @> ARRAY['Event'],false) OR r.start_at IS NOT NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.doc->>'type',ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')),o.name,NULL::text,
 o.effective_point,o.start_at,o.end_at,o.venue_id,o.document,o.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,NULL::text,NULL::bigint,
 md5(jsonb_build_array(r.id,o.uid,o.recurrence_id)::text),jsonb_build_object('uid',o.uid,'recurrence_id',nullif(o.recurrence_id,''),'event_resource_id',o.event_resource_id),NULL::jsonb,NULL::text
 FROM event_occurrence o JOIN resource r ON r.id=o.resource_id WHERE r.status<>'missing'
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,'Event',ARRAY['Thing','Event'],NULL::text,NULL::text,r.point,f.start_at,f.end_at,r.id,
 jsonb_build_object('type','Event','type_path',jsonb_build_array('Thing','Event'),'url',r.uri,'site',coalesce(r.host,''),'source','nexus_reference','status','ok','attributes','{}'::jsonb,'start',f.start_at,'end',f.end_at,'event_status',coalesce(f.event_status,'scheduled')) || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',r.doc->'location') END,
 ''::tsvector,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id,
 md5(jsonb_build_array(r.id,f.author,f.object_path,f.recurrence_id)::text),NULL::jsonb,jsonb_build_object('author',f.author,'object_path',f.object_path,'recurrence_id',f.recurrence_id,'start',f.start_at,'end',f.end_at,'status',coalesce(f.event_status,'scheduled')),f.author
 FROM reference f JOIN resource_document r ON r.id=f.resource_id WHERE f.kind='event' AND f.start_at IS NOT NULL;
