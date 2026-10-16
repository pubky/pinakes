-- Demotion is recomputed from live claims instead of a permanent tombstone. Claims stay
-- stored: `counted` marks claims that count toward demotion (not authoritative, not a
-- same-page variant) and `demoted` hides them from planning while they remain live.
ALTER TABLE resource_group ADD COLUMN counted boolean NOT NULL DEFAULT false,
 ADD COLUMN demoted boolean NOT NULL DEFAULT false;
UPDATE resource_group SET counted=true WHERE key LIKE 'url:%' AND NOT trusted AND key<>'url:'||resource_id;
ALTER TABLE group_runtime ADD COLUMN url_demotions_total bigint NOT NULL DEFAULT 0;
UPDATE group_runtime SET url_demotions_total=(SELECT count(*) FROM group_demoted_url);
-- Claimants whose rows the tombstone deleted rebuild from their documents, with owners kept.
WITH tombstoned AS (SELECT DISTINCT r.id FROM group_demoted_url d JOIN resource r ON r.doc->'group_keys' @> jsonb_build_array(jsonb_build_object('key',d.key))),
 cleared AS (DELETE FROM group_input WHERE resource_id IN(SELECT id FROM tombstoned))
INSERT INTO group_dirty(resource_id) SELECT id FROM tombstoned ON CONFLICT DO NOTHING;
DROP TABLE group_demoted_url;
-- When a counted claim disappears (document change, orphan deletion cascade), the
-- remaining demoted claimants of that key re-plan, so groups heal without operator action.
CREATE FUNCTION heal_url_demotions() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 WITH gone AS (SELECT DISTINCT key,origin FROM old_rows WHERE counted),
 woken AS (SELECT DISTINCT k.resource_id FROM resource_group k JOIN gone USING(key,origin) WHERE k.demoted),
 cleared AS (DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM woken))
 INSERT INTO group_dirty(resource_id) SELECT resource_id FROM woken ON CONFLICT DO NOTHING;
 RETURN NULL;
END $$;
CREATE TRIGGER heal_url_demotions AFTER DELETE ON resource_group REFERENCING OLD TABLE AS old_rows
 FOR EACH STATEMENT EXECUTE FUNCTION heal_url_demotions();
CREATE OR REPLACE FUNCTION refresh_group_best(ids text[]) RETURNS void LANGUAGE sql AS $$
 UPDATE "group" g SET best_unit=b.unit_id,best_resource_id=b.resource_id,best_occurrence_key=b.occurrence_key,kind=b.type,
 member_count=(SELECT count(*) FROM group_unit u WHERE u.group_id=g.id)
 FROM (SELECT DISTINCT ON(u.group_id) u.group_id,s.unit_id,u.resource_id,u.occurrence_key,
   CASE WHEN s.osm_id IS NULL THEN s.type END AS type
   FROM group_unit u JOIN "group" current ON current.id=u.group_id
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=u.unit_id OFFSET 0) s
   LEFT JOIN resource_score r ON r.resource_id=u.resource_id
   CROSS JOIN LATERAL (SELECT count(DISTINCT f.author) FILTER(WHERE f.kind='tag') AS taggers,count(*) AS refs FROM effective_reference f WHERE f.resource_id=u.resource_id) refs
   WHERE u.group_id=ANY(ids)
   ORDER BY u.group_id,(current.curated AND current.best_unit=u.unit_id) DESC,
     -- A member whose live canonical names another member is a claimant; the named page
     -- outranks it whatever its taggers. Registry and sameAs evidence follow rule 4.
     (NOT current.curated AND EXISTS(SELECT 1 FROM resource_group k WHERE k.unit_id=u.unit_id AND k.basis='canonical' AND NOT k.demoted
       AND k.key<>'url:'||u.resource_id AND EXISTS(SELECT 1 FROM group_unit o WHERE o.group_id=u.group_id AND k.key='url:'||o.resource_id))) ASC,
     refs.taggers DESC,refs.refs DESC,u.admitted_at,
     (s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,(s.name IS NOT NULL) DESC,(s.doc->>'image' IS NOT NULL) DESC,coalesce(r.score,0) DESC,s.unit_id
 ) b WHERE g.id=b.group_id;
 UPDATE "group" g SET card=v.card,card_osm_type=v.osm_type,card_osm_id=v.osm_id,card_osm_resource=v.osm_resource
 FROM (SELECT current.id,
  coalesce(b.osm_type,split_part(b.doc#>>'{venue,osm_id}','/',1)) AS osm_type,
  coalesce(b.osm_id,CASE WHEN split_part(b.doc#>>'{venue,osm_id}','/',2) ~ '^[0-9]{1,18}$' THEN split_part(b.doc#>>'{venue,osm_id}','/',2)::bigint END) AS osm_id,
  b.osm_id IS NOT NULL AS osm_resource,
  jsonb_build_object('id',current.id,'kind',current.kind,
    'name',coalesce(current.name,CASE WHEN b.osm_id IS NULL THEN b.name END),
    'image',coalesce(current.image,CASE WHEN b.osm_id IS NULL THEN b.doc->>'image' END),
    'location',CASE WHEN b.osm_id IS NULL AND b.doc#>>'{venue,osm_id}' IS NULL AND b.point IS NOT NULL THEN jsonb_build_object('lat',ST_Y(b.point),'lon',ST_X(b.point)) END,
    'attribution',CASE WHEN b.osm_id IS NULL AND b.doc#>>'{venue,osm_id}' IS NULL THEN b.doc->>'attribution' END,
    'curated',current.curated,'taggers',coalesce(s.taggers,0),'references',coalesce(s.references_count,0),'member_count',current.member_count) AS card
  FROM "group" current LEFT JOIN group_score s ON s.group_id=current.id
  LEFT JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=current.best_unit OFFSET 0) b ON true
  WHERE current.id=ANY(ids)) v WHERE g.id=v.id;
$$;
DO $$ DECLARE batch text[]; last_id text=''; BEGIN LOOP
 SELECT array_agg(id ORDER BY id) INTO batch FROM (SELECT id FROM "group" WHERE id>last_id ORDER BY id LIMIT 256) picked;
 EXIT WHEN batch IS NULL;
 PERFORM refresh_group_best(batch); last_id=batch[array_length(batch,1)];
 END LOOP; END $$;
