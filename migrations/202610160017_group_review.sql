-- Derived grouping has its own durable retry schedule and cannot fail resource saves.
ALTER TABLE group_dirty ADD COLUMN attempts integer NOT NULL DEFAULT 0,
 ADD COLUMN last_error text, ADD COLUMN next_attempt_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX group_dirty_due_idx ON group_dirty(next_attempt_at,resource_id);
ALTER TABLE group_unit ADD COLUMN occurrence jsonb, ADD COLUMN event jsonb;
UPDATE group_unit SET occurrence=jsonb_build_object('uid',occurrence_key::jsonb->>0,'recurrence_id',nullif(occurrence_key::jsonb->>1,'')) WHERE occurrence_key<>'';
UPDATE group_unit u SET event=jsonb_build_object('author',f.author,'object_path',f.object_path,'recurrence_id',f.recurrence_id,'start',f.start_at,'end',f.end_at,'status',coalesce(f.event_status,'scheduled'))
 FROM reference f WHERE f.kind='event' AND f.resource_id=u.resource_id AND u.unit_id=md5(jsonb_build_array(f.resource_id,f.author,f.object_path,f.recurrence_id)::text);
ALTER TABLE resource_group ADD COLUMN origin text NOT NULL DEFAULT '', ADD COLUMN target text,
 ADD COLUMN trusted boolean NOT NULL DEFAULT false, ADD COLUMN admitted_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX resource_group_admission_idx ON resource_group(key,trusted DESC,admitted_at,unit_id);
ALTER TABLE "group" ADD COLUMN best_resource_id text, ADD COLUMN best_occurrence_key text,
 ADD COLUMN member_count integer NOT NULL DEFAULT 0;
CREATE TABLE group_input(resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE, fingerprint text NOT NULL);

CREATE OR REPLACE FUNCTION mark_group_dirty() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings') THEN RETURN NULL; END IF;
  INSERT INTO group_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  IF TG_TABLE_NAME='reference' THEN
   IF NOT ((TG_OP<>'DELETE' AND NEW.kind='event') OR (TG_OP<>'INSERT' AND OLD.kind='event')) THEN RETURN NULL; END IF;
  END IF;
  INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;

-- Join the stored identity columns, never a computed MD5 across all search units.
CREATE VIEW group_member_document AS
 SELECT r.*,u.unit_id,u.occurrence,u.event,NULL::text AS event_author
 FROM group_unit u JOIN resource_document r ON r.id=u.resource_id WHERE u.occurrence IS NULL AND u.event IS NULL
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,o.doc->>'type',ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')),o.name,NULL::text,
 o.effective_point,o.start_at,o.end_at,o.venue_id,o.document,o.search_text,r.resolver,r.resolved_at,r.refresh_at,r.status,NULL::text,NULL::bigint,
 u.unit_id,u.occurrence,NULL::jsonb,NULL::text
 FROM group_unit u JOIN event_occurrence o ON o.resource_id=u.resource_id AND o.uid=u.occurrence->>'uid' AND o.recurrence_id=coalesce(u.occurrence->>'recurrence_id','') JOIN resource r ON r.id=u.resource_id WHERE u.occurrence IS NOT NULL AND r.status<>'missing'
 UNION ALL
 SELECT r.id,r.uri,r.scheme,r.host,'Event',ARRAY['Thing','Event'],NULL::text,NULL::text,r.point,f.start_at,f.end_at,r.id,
 jsonb_build_object('type','Event','type_path',jsonb_build_array('Thing','Event'),'url',r.uri,'site',coalesce(r.host,''),'source','nexus_reference','status','ok','attributes','{}'::jsonb,'start',f.start_at,'end',f.end_at,'event_status',coalesce(f.event_status,'scheduled')) || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location',r.doc->'location') END,
 ''::tsvector,r.resolver,r.resolved_at,r.refresh_at,r.status,r.osm_type,r.osm_id,
 u.unit_id,NULL::jsonb,u.event,f.author
 FROM group_unit u JOIN reference f ON f.resource_id=u.resource_id AND f.author=u.event->>'author' AND f.object_path=u.event->>'object_path' AND f.recurrence_id=coalesce(u.event->>'recurrence_id','') JOIN resource_document r ON r.id=u.resource_id WHERE u.event IS NOT NULL AND f.kind='event' AND f.start_at IS NOT NULL;

CREATE OR REPLACE FUNCTION group_card(gid text) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT jsonb_build_object('id',g.id,'kind',coalesce(g.kind,b.type),'name',coalesce(g.name,b.name),
 'image',coalesce(g.image,b.doc->>'image'),'attribution',b.doc->>'attribution','taggers',coalesce(s.taggers,0),'references',coalesce(s.references_count,0),'curated',g.curated,
 'members',coalesce((SELECT jsonb_agg(jsonb_build_object('resource_id',r.id,'uri',r.uri,'site',coalesce(r.host,r.scheme,'')) ||
 CASE WHEN u.occurrence IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('occurrence',u.occurrence) END ORDER BY u.unit_id)
 FROM group_unit u JOIN resource r ON r.id=u.resource_id WHERE u.group_id=g.id),'[]'::jsonb))
 FROM "group" g LEFT JOIN group_score s ON s.group_id=g.id LEFT JOIN group_member_document b ON b.unit_id=g.best_unit WHERE g.id=gid
$$;
CREATE OR REPLACE FUNCTION group_member_deleted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id);
 INSERT INTO group_dirty(resource_id) SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id ON CONFLICT DO NOTHING;
 UPDATE "group" g SET member_count=(SELECT count(*) FROM group_unit u WHERE u.group_id=g.id) WHERE id=OLD.group_id;
 UPDATE "group" g SET (best_unit,best_resource_id,best_occurrence_key)=(SELECT u.unit_id,u.resource_id,u.occurrence_key FROM group_unit u JOIN group_member_document s USING(unit_id) WHERE u.group_id=OLD.group_id ORDER BY (s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,u.unit_id LIMIT 1)
 WHERE id=OLD.group_id AND best_unit=OLD.unit_id;
 DELETE FROM "group" g WHERE id=OLD.group_id AND NOT curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id);
 RETURN NULL;
END $$;
CREATE OR REPLACE VIEW grouped_search_unit AS
 SELECT b.id,b.uri,b.scheme,b.host,coalesce(g.kind,b.type) AS type,b.type_path,coalesce(g.name,b.name) AS name,b.category,
 b.point,b.start_at,b.end_at,b.venue_id,b.doc || CASE WHEN g.name IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('name',g.name) END || CASE WHEN g.image IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('image',g.image) END AS doc,
 b.search_text,b.resolver,b.resolved_at,b.refresh_at,b.status,b.osm_type,b.osm_id,g.id AS unit_id,b.occurrence,b.event,b.event_author,NULL::jsonb AS "group"
 FROM "group" g JOIN group_member_document b ON b.unit_id=g.best_unit
 UNION ALL
 SELECT b.*,NULL::jsonb FROM search_unit b WHERE NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.unit_id=b.unit_id AND u.group_id IS NOT NULL);

-- Old unverified automatic memberships must not remain visible during the bounded backfill.
UPDATE resource_group SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
UPDATE group_unit SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
DELETE FROM "group" WHERE NOT curated;
INSERT INTO group_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now();
