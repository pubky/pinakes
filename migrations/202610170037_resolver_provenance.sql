-- The route (resource.resolver) whose worker saved the stored document. 0012/0013/0015 re-routed
-- resources without refetching them: 0013 restores the pre-0012 next_attempt_at and keeps it
-- with greatest(...), as 0015 does, so a re-routed row kept the previous resolver's card until
-- its old refresh date. Those migrations are published and stay unchanged; instead the daemon
-- re-queues, at startup, every ok row whose resolved_by differs from its resolver, which also
-- covers any later routing change.
ALTER TABLE resource ADD COLUMN resolved_by text;
-- Saved documents are attributed to their current route, except a card from the generic web
-- extractor on an identifier, media or platform route: it stays NULL, so the startup check
-- refetches it once. A legitimate fallback (negotiation failed) is then saved with
-- resolved_by set and is not re-queued again.
UPDATE resource SET resolved_by=resolver WHERE resolver IS NOT NULL AND resolved_at IS NOT NULL
 AND NOT (status='ok' AND resolver IN ('event_platform','media','nostr','isbn','doi','activitypub')
   AND coalesce(doc->>'source','web') IN ('web','opengraph','microformats','jsonld','oembed'));
