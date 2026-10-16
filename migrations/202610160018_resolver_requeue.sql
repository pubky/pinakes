-- Track which worker produced an error so future route migrations can revive it once.
ALTER TABLE resource ADD COLUMN last_error_resolver text;
UPDATE resource SET last_error_resolver=resolver WHERE last_error IS NOT NULL;
-- 0015 changed these routes before provenance existed. Requeue only its destination
-- families, once on upgrade; unrelated web/OSM/adapter dead letters remain untouched.
UPDATE resource SET attempts=0,timeout_streak=0,dead_letter=false,final=false,last_error=NULL,last_error_resolver=NULL,
 lease_until=NULL,priority_at=NULL,generation=generation+1,
 next_attempt_at=now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')
 WHERE last_error IS NOT NULL AND (attempts>0 OR dead_letter) AND (
 (resolver='media' AND host IN ('youtube.com','www.youtube.com','youtu.be','vimeo.com','www.vimeo.com','open.spotify.com','soundcloud.com')) OR
 (resolver='isbn' AND scheme='isbn') OR (resolver='doi' AND (scheme='doi' OR host='doi.org')) OR
 (resolver='nostr' AND scheme='nostr' AND uri ~ '^nostr:(npub|nprofile|note|nevent)1'));
CREATE FUNCTION remember_error_resolver() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 NEW.last_error_resolver=CASE WHEN NEW.last_error IS NULL THEN NULL ELSE NEW.resolver END;
 RETURN NEW;
END $$;
CREATE TRIGGER remember_error_resolver BEFORE UPDATE OF last_error ON resource FOR EACH ROW EXECUTE FUNCTION remember_error_resolver();
CREATE FUNCTION reroute_resource_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF NEW.resolver IS DISTINCT FROM OLD.resolver AND NEW.resolver IS NOT NULL AND OLD.last_error IS NOT NULL
 AND coalesce(OLD.last_error_resolver,OLD.resolver) IS DISTINCT FROM NEW.resolver THEN
  NEW.attempts=0; NEW.timeout_streak=0; NEW.dead_letter=false; NEW.final=false;
  NEW.last_error=NULL; NEW.last_error_resolver=NULL; NEW.lease_until=NULL; NEW.priority_at=NULL;
  NEW.generation=OLD.generation+1;
  NEW.next_attempt_at=now()+((('x'||substr(NEW.id,1,8))::bit(32)::bigint % 3600)*interval '1 second');
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER reroute_resource_failure BEFORE UPDATE OF resolver ON resource FOR EACH ROW EXECUTE FUNCTION reroute_resource_failure();
-- DOI informational pages are ordinary web pages; reclassification uses the same fence.
UPDATE resource SET resolver='web' WHERE resolver='doi' AND scheme IN('http','https') AND host='doi.org'
 AND uri !~ '^https?://doi[.]org/10[.][0-9]{4,9}/[^[:space:]]+';

-- Historical note cards from the candidate must obey the same excerpt policy.
UPDATE resource SET doc=jsonb_set(doc,'{description}',to_jsonb(left(doc->>'description',200)))
 WHERE doc->>'source' IN('nostr','activitypub') AND doc->>'type' IN('SocialMediaPosting','Article','CreativeWork') AND length(doc->>'description')>200;
UPDATE resource_change SET before=CASE WHEN length(before->>'description')>200 THEN jsonb_set(before,'{description}',to_jsonb(left(before->>'description',200))) ELSE before END,
 after=CASE WHEN length(after->>'description')>200 THEN jsonb_set(after,'{description}',to_jsonb(left(after->>'description',200))) ELSE after END
 WHERE coalesce(after->>'source',before->>'source') IN('nostr','activitypub') AND coalesce(after->>'type',before->>'type') IN('SocialMediaPosting','Article','CreativeWork');
DELETE FROM resource_change WHERE resource_id IN(SELECT id FROM resource WHERE status='missing' AND doc->>'source' IN('nostr','activitypub') AND doc->>'type' IN('SocialMediaPosting','Article','CreativeWork'));
