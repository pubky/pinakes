-- One entry per service on every card: a tagged member whose card names the same service as a
-- derived link (a tagged F-Droid page and Wikidata's F-Droid link, both "F-Droid") takes that
-- service's one place, ahead of the derived link, as a music service's tagged member always did.
-- Other tagged members stay one entry each.
CREATE OR REPLACE FUNCTION also_on(unit text, resource text, cap integer, members boolean, per_site boolean DEFAULT true) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 WITH grp AS (SELECT group_id,resource_id FROM group_unit WHERE members AND unit_id=unit AND group_id IS NOT NULL),
 -- Before any document is read: one unit for each of up to `cap` other resources (by ID), then
 -- up to `cap` more units of the card's own resource (a feed's other occurrences), and the
 -- unit itself. A resource with many units never crowds the others out.
 others AS (SELECT DISTINCT ON (u.resource_id) u.unit_id FROM grp JOIN group_unit u USING(group_id)
   WHERE u.resource_id<>grp.resource_id ORDER BY u.resource_id,u.unit_id LIMIT cap),
 siblings AS (SELECT u.unit_id FROM grp JOIN group_unit u USING(group_id,resource_id)
   WHERE u.unit_id<>unit ORDER BY u.unit_id LIMIT cap),
 picked AS (SELECT unit_id FROM others UNION SELECT unit_id FROM siblings UNION SELECT unit FROM grp),
 m AS (SELECT d.id,CASE WHEN d.occurrence IS NULL THEN d.uri ELSE coalesce(nullif(d.doc->>'url',''),d.uri) END AS uri,
   d.host,d.doc->>'site' AS site,d.unit_id=unit AS own,d.id=(SELECT resource_id FROM grp) AS mine,
   d.type IN ('MusicRecording','MusicAlbum','MusicGroup','MusicPlaylist') AS music,
   (SELECT f.value FROM resource_fact f WHERE f.resource_id=d.id AND f.name='album' AND f.expires_at>now() ORDER BY f.value LIMIT 1) AS album
   FROM picked p
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=p.unit_id OFFSET 0) d),
 tagged AS (SELECT id,uri,min(host) AS host,min(site) AS site,bool_or(mine) AS mine,bool_or(music) AS music,min(album) AS album FROM m GROUP BY id,uri HAVING NOT bool_or(own)),
 owners AS (SELECT resource AS resource_id UNION SELECT u.resource_id FROM grp JOIN group_unit u USING(group_id)),
 original AS (SELECT lower(min(f.value)) AS title FROM resource_fact f JOIN owners o USING(resource_id) WHERE f.name='original_album' AND f.expires_at>now()),
 derived AS (SELECT DISTINCT ON (l.link_id) l.link_id,l.uri,l.site,l.via,l.kind,l.at,
     count(*) OVER (PARTITION BY l.link_id) AS named
   FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND NOT EXISTS(SELECT 1 FROM owners x WHERE x.resource_id=l.link_id)
     AND NOT EXISTS(SELECT 1 FROM tagged t WHERE t.id=l.link_id)
   ORDER BY l.link_id,l.seen_at DESC,l.via),
 entries AS (SELECT CASE WHEN mine THEN 1 ELSE 0 END AS o,id AS k1,uri AS k2,
     CASE WHEN starts_with(lower(album),(SELECT title FROM original)) THEN -1 ELSE 0 END::bigint AS rank,
     CASE WHEN site IS NOT NULL AND (music OR lower(site) IN (SELECT lower(d.site) FROM derived d)) THEN lower(site) ELSE 'member:'||id END AS service,
     jsonb_build_object('id',id,'uri',uri,'host',host,'site',site) AS e FROM tagged
   UNION ALL SELECT 2,lower(site),uri,-named,lower(site),jsonb_strip_nulls(jsonb_build_object('uri',uri,'site',site,'derived',true,'via',via,'kind',kind,'at',at)) FROM derived),
 own AS (SELECT lower(r.doc->>'site') AS service FROM resource r WHERE r.id=also_on.resource),
 shown AS (SELECT * FROM (SELECT *,row_number() OVER (PARTITION BY service ORDER BY o,rank,k2) AS nth FROM entries) x
   WHERE NOT per_site OR (nth=1 AND service IS DISTINCT FROM (SELECT service FROM own)))
 SELECT coalesce((SELECT jsonb_agg(e ORDER BY o,k1,k2) FROM (SELECT * FROM shown ORDER BY o,k1,k2 LIMIT cap) x),'[]'::jsonb)
$$;
