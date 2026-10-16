-- Existing errors may contain upstream query strings. Keep only bounded classes.
UPDATE source_state SET last_error='parse' WHERE last_error IS NOT NULL AND last_error NOT IN ('dns','connect','timeout','http_4xx','http_5xx','parse','policy');
ALTER TABLE source_state ADD CONSTRAINT source_error_class CHECK(last_error IS NULL OR last_error IN ('dns','connect','timeout','http_4xx','http_5xx','parse','policy'));

-- Purge web-fetched OSM facts before reclassifying variants; resource identities stay unchanged.
DELETE FROM resource_change WHERE source<>'osm' AND resource_id IN (SELECT id FROM resource WHERE lower(rtrim(host,'.')) ~ '(^|[.])(openstreetmap[.]org|osm[.]org)$');
UPDATE resource SET name=NULL,point=NULL,category=NULL,type=NULL,type_path='{}',start_at=NULL,end_at=NULL,venue_id=NULL,doc='{}',status='error',resolved_at=NULL,refresh_at=NULL,attempts=0,dead_letter=true,lease_until=NULL,priority_at=NULL,next_attempt_at='infinity',final=true,ever_existed=false,last_error='policy'
 WHERE resolver='web' AND lower(rtrim(host,'.')) ~ '(^|[.])(openstreetmap[.]org|osm[.]org)$';
WITH aliases AS (SELECT id,regexp_match(uri,'^https?://(?:www[.])?(?:openstreetmap[.]org|osm[.]org)[.]?/(node|way|relation)/([1-9][0-9]{0,18})/?(?:[?].*)?$') AS m FROM resource WHERE resolver='web')
UPDATE resource r SET resolver='osm',osm_type=a.m[1],osm_id=a.m[2]::bigint,type_path=NULL,doc=jsonb_build_object('type','Place','url',r.uri,'osm_id',a.m[1]||'/'||a.m[2],'source','osm','status','pending'),status='pending',dead_letter=false,final=false,next_attempt_at=now(),last_error=NULL
 FROM aliases a WHERE a.id=r.id AND a.m IS NOT NULL AND a.m[2]::numeric<=9223372036854775807;
UPDATE resource SET doc=jsonb_build_object('type','Place','url',uri,'osm_id',osm_type||'/'||osm_id,'source','osm','status',status) WHERE resolver='osm';
ALTER TABLE resource DROP CONSTRAINT resource_osm_separation;
ALTER TABLE resource ADD CONSTRAINT resource_osm_separation CHECK (
 (osm_id IS NULL AND osm_type IS NULL) OR
 (osm_id IS NOT NULL AND osm_type IS NOT NULL AND osm_id>0 AND osm_type IN ('node','way','relation') AND resolver IS NOT DISTINCT FROM 'osm'
  AND name IS NULL AND point IS NULL AND category IS NULL AND type IS NULL AND type_path IS NULL
  AND doc->>'type' IS NOT DISTINCT FROM 'Place' AND doc-ARRAY['type','url','osm_id','source','status']::text[]='{}'::jsonb));
ALTER TABLE resource ADD CONSTRAINT resource_web_osm_boundary CHECK (
 resolver IS DISTINCT FROM 'web' OR lower(rtrim(host,'.')) !~ '(^|[.])(openstreetmap[.]org|osm[.]org)$'
 OR (name IS NULL AND point IS NULL AND category IS NULL AND type IS NULL AND doc='{}'::jsonb));
