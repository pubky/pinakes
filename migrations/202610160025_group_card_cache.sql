ALTER TABLE "group" ADD COLUMN card jsonb,
 ADD COLUMN card_osm_type text, ADD COLUMN card_osm_id bigint, ADD COLUMN card_osm_resource boolean NOT NULL DEFAULT false;
-- Base unit identity is indexed; OFFSET 0 keeps the hydration parameterized instead of
-- flattening the UNION view into a scan of every member for every candidate.
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
-- Ordinary cards are a cached-column read. ODbL facts remain solely in osm_place:
-- the exceptional OSM branch is two primary-key lookups, never member-view hydration.
CREATE OR REPLACE FUNCTION group_card(gid text) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT CASE WHEN g.card_osm_id IS NULL THEN g.card ELSE g.card ||
  jsonb_build_object('location',o.doc->'location','attribution','© OpenStreetMap contributors (ODbL)') ||
  CASE WHEN g.card_osm_resource THEN jsonb_build_object('kind',o.type,'name',coalesce(g.name,o.name),'image',coalesce(g.image,o.doc->>'image')) ELSE '{}'::jsonb END END
 FROM "group" g LEFT JOIN osm_place o ON o.osm_type=g.card_osm_type AND o.osm_id=g.card_osm_id WHERE g.id=gid
$$;
CREATE FUNCTION group_detail(gid text) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT group_card(gid) || jsonb_build_object('members',coalesce((SELECT jsonb_agg(
  jsonb_build_object('resource_id',r.id,'uri',r.uri,'site',coalesce(r.host,r.scheme,'')) ||
  CASE WHEN u.occurrence IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('occurrence',u.occurrence) END ORDER BY u.unit_id)
  FROM group_unit u JOIN resource r ON r.id=u.resource_id WHERE u.group_id=gid),'[]'::jsonb))
$$;
-- Rebuild in bounded batches on upgrade; steady-state refresh uses only affected groups.
DO $$ DECLARE batch text[]; last_id text=''; BEGIN LOOP
 SELECT array_agg(id ORDER BY id) INTO batch FROM (SELECT id FROM "group" WHERE id>last_id ORDER BY id LIMIT 256) picked;
 EXIT WHEN batch IS NULL;
 PERFORM refresh_group_best(batch); last_id=batch[array_length(batch,1)];
 END LOOP; END $$;
