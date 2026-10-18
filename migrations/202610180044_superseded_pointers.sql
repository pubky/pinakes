-- Superseded rows carry no supersedes pointer (the upsert clears it when a row is superseded, so
-- a flip-back leaves no cycle). Clear the pointers superseded rows kept from earlier releases,
-- in batches of 1000.
DO $$
DECLARE cleared integer;
BEGIN
  LOOP
    UPDATE occurrence SET supersedes=NULL
     WHERE (resource_id,uid,recurrence_id) IN (
       SELECT resource_id,uid,recurrence_id FROM occurrence
        WHERE status='superseded' AND supersedes IS NOT NULL LIMIT 1000);
    GET DIAGNOSTICS cleared = ROW_COUNT;
    EXIT WHEN cleared = 0;
  END LOOP;
END $$;
