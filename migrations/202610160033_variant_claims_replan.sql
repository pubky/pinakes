-- Same-page variants are now an allowlist of leading segments (/amp, /m, /mobile) plus
-- equal or extending paths. The earlier suffix rule exempted /~evil/article as a variant
-- of /article. 0031's backfill marked every non-own URL claim counted without excluding
-- variants at all. Both are settled by the grouping code itself: every resource holding a
-- non-authoritative URL claim re-plans once and recomputes its counted flags.
WITH claimants AS (SELECT DISTINCT resource_id FROM resource_group WHERE key LIKE 'url:%' AND NOT trusted),
 cleared AS (DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM claimants))
INSERT INTO group_dirty(resource_id) SELECT resource_id FROM claimants ON CONFLICT DO NOTHING;
