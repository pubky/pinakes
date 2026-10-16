ALTER TABLE resource DROP CONSTRAINT resource_status_check;
ALTER TABLE resource ADD CONSTRAINT resource_status_check CHECK(status IN('ok','pending','missing','unreachable','blocked','error'));
-- One-time staggered repair, limited to known interstitial names. Queue concurrency and
-- per-host budgets bound the refetch work; existing leases are fenced before requeue.
UPDATE resource SET attempts=0,timeout_streak=0,dead_letter=false,final=false,
 last_error=NULL,last_error_resolver=NULL,lease_until=NULL,priority_at=NULL,generation=generation+1,
 next_attempt_at=now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')
 WHERE name ~* '(request access|just a moment|attention required|access denied|verify you are human|enable javascript|unusual traffic|^redirecting)'
 AND resolver IN('web','media','event','event_platform');
