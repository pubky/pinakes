-- Publisher-scoped evidence and reference-first automatic cards invalidate old admissions.
ALTER TABLE group_unit ADD COLUMN admitted_at timestamptz NOT NULL DEFAULT now();
UPDATE group_unit u SET admitted_at=coalesce((SELECT min(k.admitted_at) FROM resource_group k WHERE k.unit_id=u.unit_id),(SELECT created_at FROM resource WHERE id=u.resource_id));
CREATE FUNCTION refresh_group_best(ids text[]) RETURNS void LANGUAGE sql AS $$
 UPDATE "group" g SET best_unit=b.unit_id,best_resource_id=b.resource_id,best_occurrence_key=b.occurrence_key,kind=b.type,
 member_count=(SELECT count(*) FROM group_unit u WHERE u.group_id=g.id)
 FROM (SELECT DISTINCT ON(u.group_id) u.group_id,s.unit_id,u.resource_id,u.occurrence_key,s.type
   FROM group_unit u JOIN "group" current ON current.id=u.group_id
   JOIN group_member_document s USING(unit_id) LEFT JOIN resource_score r ON r.resource_id=u.resource_id
   CROSS JOIN LATERAL (SELECT count(DISTINCT f.author) FILTER(WHERE f.kind='tag') AS taggers,count(*) AS refs FROM effective_reference f WHERE f.resource_id=u.resource_id) refs
   WHERE u.group_id=ANY(ids)
   ORDER BY u.group_id,(current.curated AND current.best_unit=u.unit_id) DESC,
     refs.taggers DESC,refs.refs DESC,u.admitted_at,
     (s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,(s.name IS NOT NULL) DESC,(s.doc->>'image' IS NOT NULL) DESC,coalesce(r.score,0) DESC,s.unit_id
 ) b WHERE g.id=b.group_id
$$;
CREATE OR REPLACE FUNCTION group_card(gid text) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT jsonb_build_object('id',g.id,'kind',coalesce(g.kind,b.type),'name',coalesce(g.name,b.name),
 'image',coalesce(g.image,b.doc->>'image'),'location',CASE WHEN b.point IS NOT NULL THEN jsonb_build_object('lat',ST_Y(b.point),'lon',ST_X(b.point)) END,'attribution',b.doc->>'attribution','taggers',coalesce(s.taggers,0),'references',coalesce(s.references_count,0),'curated',g.curated,
 'members',coalesce((SELECT jsonb_agg(jsonb_build_object('resource_id',r.id,'uri',r.uri,'site',coalesce(r.host,r.scheme,'')) ||
 CASE WHEN u.occurrence IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('occurrence',u.occurrence) END ORDER BY u.unit_id)
 FROM group_unit u JOIN resource r ON r.id=u.resource_id WHERE u.group_id=g.id),'[]'::jsonb))
 FROM "group" g LEFT JOIN group_score s ON s.group_id=g.id LEFT JOIN group_member_document b ON b.unit_id=g.best_unit WHERE g.id=gid
$$;
CREATE OR REPLACE FUNCTION group_member_deleted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id);
 INSERT INTO group_dirty(resource_id) SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id ON CONFLICT DO NOTHING;
 PERFORM refresh_group_best(ARRAY[OLD.group_id]);
 DELETE FROM "group" g WHERE id=OLD.group_id AND NOT curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id);
 RETURN NULL;
END $$;
CREATE OR REPLACE FUNCTION mark_group_dirty() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings') THEN RETURN NULL; END IF;
  INSERT INTO group_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;

DELETE FROM group_input;
UPDATE resource_group SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
UPDATE group_unit SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
DELETE FROM "group" WHERE NOT curated;
INSERT INTO group_dirty(resource_id) SELECT id FROM resource ON CONFLICT(resource_id) DO UPDATE SET attempts=0,last_error=NULL,next_attempt_at=now();
