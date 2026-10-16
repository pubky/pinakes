-- Body <link rel=canonical> (user content on some sites) used to mint canonical keys. The
-- extractor now reads head links only, but stored documents keep the old keys and a re-plan
-- would reuse them. Resources holding a non-authoritative canonical claim (web pages and the
-- event, platform and card sources that fall back to web extraction) therefore refetch
-- once through the normal bounded workers, staggered over six hours, with existing leases
-- fenced by the generation bump. Their group re-plan follows from the new document.
UPDATE resource r SET attempts=0,timeout_streak=0,dead_letter=false,final=false,
 last_error=NULL,last_error_resolver=NULL,lease_until=NULL,priority_at=NULL,generation=generation+1,
 next_attempt_at=least(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 21600)*interval '1 second'))
 WHERE status='ok'
 AND EXISTS(SELECT 1 FROM resource_group k WHERE k.resource_id=r.id AND k.basis='canonical' AND NOT k.trusted);
