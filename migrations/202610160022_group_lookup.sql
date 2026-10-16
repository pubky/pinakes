CREATE OR REPLACE VIEW group_member_document AS
 SELECT r.*,u.unit_id,u.occurrence,u.event,NULL::text AS event_author
 FROM group_unit u CROSS JOIN LATERAL (SELECT * FROM resource_document WHERE id=u.resource_id OFFSET 0) r WHERE u.occurrence IS NULL AND u.event IS NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.doc->>'type',ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')),o.name,NULL::text,
 o.effective_point,o.start_at,o.end_at,o.venue_id,o.document,o.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,NULL::text,NULL::bigint,
 u.unit_id,u.occurrence,NULL::jsonb,NULL::text
 FROM group_unit u CROSS JOIN LATERAL (SELECT * FROM event_occurrence WHERE resource_id=u.resource_id AND uid=u.occurrence->>'uid' AND recurrence_id=coalesce(u.occurrence->>'recurrence_id','') OFFSET 0) o JOIN resource r ON r.id=u.resource_id WHERE u.occurrence IS NOT NULL AND r.status<>'missing'
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,'Event',ARRAY['Thing','Event'],NULL::text,NULL::text,r.point,f.start_at,f.end_at,r.id,
 jsonb_build_object('type','Event','type_path',jsonb_build_array('Thing','Event'),'url',r.uri,'site',coalesce(r.host,''),'source','nexus_reference','status','ok','attributes','{}'::jsonb,'start',f.start_at,'end',f.end_at,'event_status',coalesce(f.event_status,'scheduled')) || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',r.doc->'location') END,
 ''::tsvector,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id,
 u.unit_id,NULL::jsonb,u.event,f.author
 FROM group_unit u JOIN reference f ON f.resource_id=u.resource_id AND f.author=u.event->>'author' AND f.object_path=u.event->>'object_path' AND f.recurrence_id=coalesce(u.event->>'recurrence_id','') CROSS JOIN LATERAL (SELECT * FROM resource_document WHERE id=u.resource_id OFFSET 0) r WHERE u.event IS NOT NULL AND f.kind='event' AND f.start_at IS NOT NULL;

-- Separate grouped hydration from ungrouped eligible rows so each lookup is parameterized.
CREATE VIEW group_search_unit AS
 SELECT b.id,b.uri,b.scheme,b.host,coalesce(g.kind,b.type) AS type,b.type_path,coalesce(g.name,b.name) AS name,b.category,
 b.point,b.start_at,b.end_at,b.venue_id,b.doc || CASE WHEN g.name IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('name',g.name) END || CASE WHEN g.image IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('image',g.image) END AS doc,
 b.search_text,b.resolver,b.resolved_at,b.refresh_at,b.status,b.osm_type,b.osm_id,g.id AS unit_id,b.occurrence,b.event,b.event_author,NULL::jsonb AS "group"
 FROM "group" g CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=g.best_unit OFFSET 0) b
;
