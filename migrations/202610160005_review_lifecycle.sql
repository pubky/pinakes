CREATE INDEX resource_change_osm_idx ON resource_change(osm_type, osm_id) WHERE osm_type IS NOT NULL;
CREATE INDEX resource_claim_priority_idx ON resource(resolver, priority_at DESC NULLS LAST, next_attempt_at) WHERE NOT dead_letter AND NOT final;
CREATE INDEX resource_orphaned_idx ON resource(orphaned_at) WHERE orphaned_at IS NOT NULL;
ALTER TABLE resource_change ALTER COLUMN at SET DEFAULT clock_timestamp();
ALTER TABLE resource ADD COLUMN ever_existed boolean NOT NULL DEFAULT false;
UPDATE resource SET ever_existed=true WHERE doc->>'status'='ok' OR EXISTS(SELECT 1 FROM resource_change c WHERE c.resource_id=resource.id AND c.class IN ('created','uncancelled') AND c.after->>'status'='ok');
