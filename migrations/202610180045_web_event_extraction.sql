-- The web worker builds calendars (feeds that fail `is_feed`) and event pages with the event
-- service, but recorded extracted_by='web', so event extractor bumps never re-queued them. Record
-- them as the event extractor's output at version 0: the startup re-extraction pass then clears
-- their validators and re-queues them within its window. A calendar is a resource with an
-- event_fetch row; an event page has Event in its type path. Each statement marks at most 1000
-- rows; all of them stay locked until this migration's transaction commits.
DO $$
DECLARE marked integer;
BEGIN
  LOOP
    UPDATE resource SET extracted_by='event', extracted_with=0
     WHERE id IN (
       SELECT r.id FROM resource r
        WHERE r.status='ok' AND coalesce(r.extracted_by,r.resolver)='web'
          AND (coalesce(r.type_path,'{}') @> ARRAY['Event']
               OR EXISTS (SELECT 1 FROM event_fetch e WHERE e.resource_id=r.id))
        LIMIT 1000);
    GET DIAGNOSTICS marked = ROW_COUNT;
    EXIT WHEN marked = 0;
  END LOOP;
END $$;
