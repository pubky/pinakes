-- 0035 refetched only resources holding a non-authoritative canonical claim. Pages whose
-- stored URL claims came from body JSON-LD sameAs, same-origin body canonicals (trusted, so
-- 0035 skipped them) and claimants with double-encoded or path-parameter segments still carry
-- documents or counted flags from before the head-only and variant fixes. Every resource
-- holding a canonical or sameAs URL claim other than its own URI refetches once (registry keys
-- are untouched), in batches of 1000, staggered over six hours and never later than its
-- scheduled refresh, with existing leases fenced by the generation bump. The group re-plan
-- follows from saving the new document.
DO $$ DECLARE batch text[]; last_id text=''; BEGIN LOOP
 SELECT array_agg(id ORDER BY id) INTO batch FROM (
   SELECT DISTINCT k.resource_id AS id FROM resource_group k JOIN resource r ON r.id=k.resource_id
   WHERE k.resource_id>last_id AND r.status='ok' AND k.key LIKE 'url:%'
     AND k.basis IN ('canonical','same_as') AND k.key<>'url:'||k.resource_id
   ORDER BY 1 LIMIT 1000) picked;
 EXIT WHEN batch IS NULL;
 UPDATE resource SET attempts=0,timeout_streak=0,dead_letter=false,final=false,
  last_error=NULL,last_error_resolver=NULL,lease_until=NULL,priority_at=NULL,generation=generation+1,
  next_attempt_at=least(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 21600)*interval '1 second'))
  WHERE id=ANY(batch);
 last_id=batch[array_length(batch,1)];
 END LOOP; END $$;
