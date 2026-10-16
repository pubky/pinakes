-- Shared landing pages are aliases, not durable identity. Keep a small tombstone so
-- stale metadata cannot reintroduce a demoted key on the next reconciliation.
CREATE TABLE group_demoted_url(key text NOT NULL,origin text NOT NULL,demoted_at timestamptz NOT NULL DEFAULT now(),PRIMARY KEY(key,origin));
INSERT INTO group_demoted_url(key,origin)
 SELECT k.key,k.origin FROM resource_group k JOIN resource r ON r.id=k.resource_id
 WHERE k.key LIKE 'url:%' GROUP BY k.key,k.origin
 HAVING count(DISTINCT k.resource_id)>2 AND count(DISTINCT substring(r.uri from '^https?://[^/]+([^?#]*)'))>2;
-- Every old automatic alias admission must be reconsidered: old rows do not distinguish
-- implicit alias keys from explicit canonicals. Resource metadata and aliases are retained.
DELETE FROM resource_group k USING group_demoted_url d WHERE k.key=d.key AND k.origin=d.origin;
DELETE FROM group_input;
UPDATE resource_group SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
UPDATE group_unit SET group_id=NULL WHERE group_id IN(SELECT id FROM "group" WHERE NOT curated);
DELETE FROM "group" WHERE NOT curated;
INSERT INTO group_dirty(resource_id) SELECT id FROM resource ON CONFLICT DO NOTHING;
