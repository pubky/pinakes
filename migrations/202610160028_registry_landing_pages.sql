-- DOI/ISBN cards resolved before registry landing pages were recorded cannot group with
-- their publisher pages. Refetch them once through the normal bounded workers, staggered
-- over one hour; existing leases are fenced by the generation bump.
UPDATE resource SET attempts=0,timeout_streak=0,dead_letter=false,final=false,
 last_error=NULL,last_error_resolver=NULL,lease_until=NULL,priority_at=NULL,generation=generation+1,
 next_attempt_at=now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')
 WHERE status='ok' AND doc->>'source' IN('doi','isbn')
 AND NOT coalesce(doc->'group_keys','[]') @> '[{"basis":"registry"}]';
