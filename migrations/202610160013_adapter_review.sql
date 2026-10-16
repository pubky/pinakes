ALTER TABLE event_fetch ADD COLUMN adapter_resync boolean NOT NULL DEFAULT false;
-- URI identities and references remain immutable. Only a canonical root owns
-- cursor state and occurrences; aliases contribute the referring authors/tags.
CREATE TABLE cursor_alias (
 alias_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
 root_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
 CHECK(alias_id<>root_id)
);
CREATE INDEX cursor_alias_root ON cursor_alias(root_id);
CREATE TABLE resource_derivation (
 parent_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
 child_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
 adapter text NOT NULL,
 PRIMARY KEY(parent_id,child_id), CHECK(parent_id<>child_id)
);
CREATE INDEX resource_derivation_child ON resource_derivation(child_id);
CREATE VIEW effective_reference AS
 SELECT resource_id,author,object_path,kind,label,created_at FROM reference
 UNION
 SELECT a.root_id,f.author,f.object_path,f.kind,f.label,f.created_at
 FROM cursor_alias a JOIN reference f ON f.resource_id=a.alias_id;

-- The daemon snapshots pre-0012 queue state before applying the sealed migration.
-- Restore it here without altering its checksum. Already-upgraded installations
-- retain their current attempt/dead-letter/final state (lost history is unknowable).
DO $$ BEGIN
 IF to_regclass('adapter_routing_backup') IS NOT NULL THEN
  UPDATE resource r SET attempts=b.attempts,dead_letter=b.dead_letter,final=b.final,
    next_attempt_at=b.next_attempt_at
  FROM adapter_routing_backup b WHERE r.id=b.id;
  DROP TABLE adapter_routing_backup;
 END IF;
END $$;
-- Undo broad matches which are now deliberately ordinary web pages.
UPDATE resource SET resolver='web',next_attempt_at=CASE WHEN final OR dead_letter THEN next_attempt_at ELSE greatest(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')) END WHERE resolver='event_platform' AND (
 host='wikipedia.org' OR host LIKE '%.wikipedia.org'
 OR (host IN ('api.btcmap.org','portal.einundzwanzig.space')
     AND uri !~ '^https?://api[.]btcmap[.]org/v4/events([/?#]|$)'
     AND uri !~ '^https?://portal[.]einundzwanzig[.]space/(api/(changes|meetup-events)([?#]|$)|meetup/[^/]+/event/[0-9]+([?#]|$))')
 OR ((host='facebook.com' OR host LIKE '%.facebook.com') AND uri !~ '/events/')
 OR (host IN ('zoom.us','zoom.com','meet.google.com','teams.microsoft.com','teams.live.com') AND uri !~ '/(j/|w/|my/|l/meetup-join/)')
 OR (host='calendar.google.com' AND uri !~ '/calendar/ical/')
 OR ((host='icloud.com' OR host LIKE '%.icloud.com') AND uri !~ '/published/')
 OR (host IN ('outlook.office365.com','outlook.live.com') AND uri !~ '/owa/(0/)?calendar/')
);
UPDATE resource SET next_attempt_at=greatest(next_attempt_at,
 now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600) * interval '1 second'))
 WHERE resolver='event_platform' AND NOT final AND NOT dead_letter;
