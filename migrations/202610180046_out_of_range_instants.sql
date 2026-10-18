-- Instants outside UTC years 0001–9999 that older builds stored through binary binds, or that
-- Postgres rounded into year 10000 from a sub-microsecond value, cannot be read back through
-- their text form and fail the casts of later applies. Drop occurrences whose start is out of
-- range, and clear out-of-range occurrence ends and modification times and resource starts and
-- ends (with the same fields of their documents), as the current code does before every save.
-- Each statement changes at most 1000 rows; all of them stay locked until this migration's
-- transaction commits.
DO $$
DECLARE
  lo constant timestamptz := make_timestamptz(1,1,1,0,0,0,'UTC');
  hi constant timestamptz := make_timestamptz(10000,1,1,0,0,0,'UTC');
  changed integer;
BEGIN
  LOOP
    DELETE FROM occurrence WHERE (resource_id,uid,recurrence_id) IN (
      SELECT resource_id,uid,recurrence_id FROM occurrence
       WHERE start_at<lo OR start_at>=hi LIMIT 1000);
    GET DIAGNOSTICS changed = ROW_COUNT;
    EXIT WHEN changed = 0;
  END LOOP;
  LOOP
    UPDATE occurrence SET end_at=NULL, doc=doc-'end' WHERE (resource_id,uid,recurrence_id) IN (
      SELECT resource_id,uid,recurrence_id FROM occurrence
       WHERE end_at<lo OR end_at>=hi LIMIT 1000);
    GET DIAGNOSTICS changed = ROW_COUNT;
    EXIT WHEN changed = 0;
  END LOOP;
  LOOP
    UPDATE occurrence SET modified=NULL WHERE (resource_id,uid,recurrence_id) IN (
      SELECT resource_id,uid,recurrence_id FROM occurrence
       WHERE modified<lo OR modified>=hi LIMIT 1000);
    GET DIAGNOSTICS changed = ROW_COUNT;
    EXIT WHEN changed = 0;
  END LOOP;
  LOOP
    UPDATE resource SET start_at=NULL, doc=doc-'start' WHERE id IN (
      SELECT id FROM resource WHERE start_at<lo OR start_at>=hi LIMIT 1000);
    GET DIAGNOSTICS changed = ROW_COUNT;
    EXIT WHEN changed = 0;
  END LOOP;
  LOOP
    UPDATE resource SET end_at=NULL, doc=doc-'end' WHERE id IN (
      SELECT id FROM resource WHERE end_at<lo OR end_at>=hi LIMIT 1000);
    GET DIAGNOSTICS changed = ROW_COUNT;
    EXIT WHEN changed = 0;
  END LOOP;
END $$;
