-- Sponsorship has a bounded live window for cursor-derived event pages. Immutable
-- resource IDs/reference rows are unaffected; orphan retention remains independent.
ALTER TABLE resource_derivation ADD COLUMN expires_at timestamptz;
UPDATE resource_derivation d SET expires_at=coalesce(
 (SELECT max(coalesce(o.end_at,o.start_at)) FROM occurrence o
  WHERE o.resource_id=d.parent_id AND o.event_resource_id=d.child_id),now())
 WHERE adapter='btcmap';
CREATE INDEX resource_derivation_adapter_expiry ON resource_derivation(adapter,expires_at);
