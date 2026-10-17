-- An identifier card no longer ranks below a page on its own registry origin (an ISBN card and
-- its Open Library edition page), and that page ranks below the card: the Book card represents
-- the group. Registrant landing pages (DOI) still outrank their cards.
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
     -- A member that names another member is a claimant, and the named page outranks it
     -- whatever its taggers: an identifier card below the landing page its registry names
     -- (unless that page is on the card's own registry origin: an Open Library edition page is
     -- the registry's own record, not a registrant-chosen landing page), a canonical claimant
     -- (an occurrence naming its event page included) below the page it names, and a sameAs
     -- claimant likewise, except below an identifier-registry resource on another origin (a
     -- publisher's sameAs to doi.org or Wikidata keeps the publisher) or a member that names
     -- it back (reciprocal sameAs pairs are peers, not claimants).
     (NOT current.curated AND EXISTS(SELECT 1 FROM resource_group k JOIN group_unit o ON o.group_id=u.group_id AND k.key='url:'||o.resource_id
       WHERE k.unit_id=u.unit_id AND NOT k.demoted AND k.key<>'url:'||u.resource_id
       AND ((k.basis='registry' AND NOT EXISTS(SELECT 1 FROM resource_group ko WHERE ko.unit_id=o.unit_id AND ko.origin=k.origin))
         OR k.basis='canonical' OR (k.basis='same_as'
         AND NOT EXISTS(SELECT 1 FROM resource_group ko WHERE ko.unit_id=o.unit_id AND ko.origin<>k.origin
           AND ko.origin IN ('doi.org','openlibrary.org','wikidata.org'))
         AND NOT EXISTS(SELECT 1 FROM resource_group kb WHERE kb.unit_id=o.unit_id AND kb.key='url:'||u.resource_id
           AND kb.basis IN ('same_as','canonical') AND NOT kb.demoted))))) ASC,
     -- The registry's own page (an Open Library edition page) defers to the identifier card that
     -- names it: both describe one record, and the card is the typed one. Without this an earlier
     -- admitted page with as many taggers would keep the card through admission order.
     (NOT current.curated AND EXISTS(SELECT 1 FROM resource_group k JOIN group_unit c ON c.unit_id=k.unit_id AND c.group_id=u.group_id
       WHERE k.key='url:'||u.resource_id AND k.basis='registry' AND NOT k.demoted AND k.unit_id<>u.unit_id
       AND EXISTS(SELECT 1 FROM resource_group ko WHERE ko.unit_id=u.unit_id AND ko.origin=k.origin))) ASC,
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
 SELECT array_agg(id ORDER BY id) INTO batch FROM (SELECT DISTINCT u.group_id AS id FROM group_unit u
   JOIN resource_group k ON k.unit_id=u.unit_id AND k.basis='registry'
   WHERE u.group_id>last_id ORDER BY 1 LIMIT 256) picked;
 EXIT WHEN batch IS NULL;
 PERFORM refresh_group_best(batch); last_id=batch[array_length(batch,1)];
 END LOOP; END $$;
