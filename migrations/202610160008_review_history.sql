-- Deleted legacy resources can still have web-fetched OSM facts in the retained feed.
DELETE FROM resource_change WHERE source<>'osm' AND (
 coalesce(before->>'url','') ~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'
 OR coalesce(after->>'url','') ~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)');
ALTER TABLE resource_change ADD CONSTRAINT change_web_osm_boundary CHECK(source='osm' OR (
 coalesce(before->>'url','') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'
 AND coalesce(after->>'url','') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'));
-- For OSM the change view hydrates the status from the derivative snapshot.
UPDATE resource r SET ever_existed=true WHERE NOT ever_existed AND EXISTS(
 SELECT 1 FROM change_document c WHERE c.resource_id=r.id AND c.class IN ('created','uncancelled') AND c.after->>'status'='ok');
