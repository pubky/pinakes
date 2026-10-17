-- Location links (OSM marker links and shortlinks, OsmAnd, Organic Maps) on OSM hosts resolve as
-- geo points named by the URL itself, not as OSM data: their change lines may carry the link.
ALTER TABLE resource_change DROP CONSTRAINT change_web_osm_boundary;
ALTER TABLE resource_change ADD CONSTRAINT change_web_osm_boundary CHECK(source IN ('osm','geo') OR (
 coalesce(before->>'url','') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'
 AND coalesce(after->>'url','') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'));
-- Web refuses OSM-host pages that are neither objects nor location links before any fetch; that
-- refusal is now stored once as final ('unsupported') instead of as a dead letter. Rows the new
-- routes cover are re-routed at startup, where the reroute trigger revives them.
UPDATE resource SET dead_letter=false,final=true,last_error='unsupported',lease_until=NULL,priority_at=NULL,next_attempt_at='infinity'
 WHERE resolver='web' AND dead_letter AND last_error='policy' AND lower(rtrim(host,'.')) ~ '(^|[.])(openstreetmap[.]org|osm[.]org)$';
