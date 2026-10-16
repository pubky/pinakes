-- Until each resource is rebuilt, old publisher scopes must never connect feed tenants.
UPDATE resource_group SET origin='stale:'||unit_id
 WHERE basis IN('ics_uid','podcast_guid','podcast_series_guid')
 OR unit_id IN(SELECT unit_id FROM group_unit WHERE occurrence IS NOT NULL);
DELETE FROM group_input;
INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING;
-- Repair pre-provenance podcast episodes through normal bounded workers, staggered for 1 h.
UPDATE resource SET attempts=0,timeout_streak=0,dead_letter=false,final=false,
 last_error=NULL,last_error_resolver=NULL,lease_until=NULL,priority_at=NULL,generation=generation+1,
 next_attempt_at=now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')
 WHERE type IN('PodcastEpisode','PodcastSeries') AND doc#>>'{attributes,podcast_feed}' IS NULL;
