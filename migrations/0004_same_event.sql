-- The same-event hint: event units (an event resource, or one occurrence of a feed) on different
-- pages that are possibly the same event. A hint only: never a group, never counted, never ranks.
-- A background pass (same_event::drain) writes the pairs; cards only read them.

-- A unit the pass has read: what its pairs were computed from (`fingerprint`: start, place,
-- venue, organizer and name) and its start, so it leaves when it falls out of the window.
CREATE TABLE same_event_unit (
    unit_id text PRIMARY KEY,
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    uid text,
    recurrence_id text,
    fingerprint text NOT NULL,
    start_at timestamp with time zone NOT NULL
);
CREATE INDEX same_event_unit_resource_idx ON same_event_unit USING btree (resource_id);
CREATE INDEX same_event_unit_start_idx ON same_event_unit USING btree (start_at);

-- One row per direction of a pair: `score` is the similarity of the two normalised names.
CREATE TABLE same_event (
    unit_id text NOT NULL REFERENCES same_event_unit(unit_id) ON DELETE CASCADE,
    other_id text NOT NULL REFERENCES same_event_unit(unit_id) ON DELETE CASCADE,
    score real NOT NULL,
    PRIMARY KEY (unit_id, other_id),
    CONSTRAINT same_event_pair CHECK (unit_id <> other_id)
);
CREATE INDEX same_event_other_idx ON same_event USING btree (other_id);

-- Resources whose event units may have changed (start, place or name), or entered the window.
CREATE TABLE same_event_dirty (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE INDEX same_event_dirty_due_idx ON same_event_dirty USING btree (next_attempt_at, resource_id);

CREATE FUNCTION mark_same_event_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.name,NEW.start_at,NEW.point,NEW.venue_id,NEW.type_path,NEW.doc-'fetched_at'-'ttl_secs'-'warnings')
    IS NOT DISTINCT FROM (OLD.name,OLD.start_at,OLD.point,OLD.venue_id,OLD.type_path,OLD.doc-'fetched_at'-'ttl_secs'-'warnings') THEN RETURN NULL; END IF;
  INSERT INTO same_event_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSIF TG_OP='DELETE' THEN
  INSERT INTO same_event_dirty(resource_id) SELECT id FROM resource WHERE id=OLD.resource_id ON CONFLICT DO NOTHING;
 ELSE
  IF TG_OP='UPDATE' AND (NEW.name,NEW.start_at,NEW.point,NEW.venue_id,NEW.status,NEW.doc)
    IS NOT DISTINCT FROM (OLD.name,OLD.start_at,OLD.point,OLD.venue_id,OLD.status,OLD.doc) THEN RETURN NULL; END IF;
  INSERT INTO same_event_dirty(resource_id) VALUES(NEW.resource_id) ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;

CREATE TRIGGER same_event_resource_dirty AFTER INSERT OR UPDATE ON resource FOR EACH ROW EXECUTE FUNCTION mark_same_event_dirty();
CREATE TRIGGER same_event_occurrence_dirty AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION mark_same_event_dirty();

-- An event name as the hint compares it: lower case, without punctuation, symbols and emoji,
-- without dates (any word with a digit, month and weekday names) and filler words such as
-- "meetup" and "event".
CREATE FUNCTION same_event_name(name text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT btrim(regexp_replace(regexp_replace(regexp_replace(regexp_replace(lower(coalesce(name,'')),
  '[!-/:-@[-`{-~\u00a0-\u00bf\u00d7\u00f7\u2000-\u2bff\u3000-\u303f\ufe00-\ufe0f\U0001f000-\U0001faff]+',' ','g'),
  '\S*[0-9]\S*',' ','g'),
  '\m(meetups?|meet ?ups?|events?|edition|session|monthly|weekly|stammtisch|treffen|rencontre|incontro|jan|january|januar|janvier|gennaio|feb|february|februar|fevrier|février|febbraio|mar|march|maerz|märz|marz|mars|marzo|apr|april|avril|aprile|may|mai|maggio|jun|june|juni|juin|giugno|jul|july|juli|juillet|luglio|aug|august|aout|août|agosto|sep|sept|september|septembre|settembre|oct|october|okt|oktober|octobre|ottobre|nov|november|novembre|dec|december|dez|dezember|decembre|décembre|dicembre|mon|monday|montag|lundi|tue|tuesday|dienstag|mardi|wed|wednesday|mittwoch|mercredi|thu|thursday|donnerstag|jeudi|fri|friday|freitag|vendredi|sat|saturday|samstag|samedi|sun|sunday|sonntag|dimanche)\M',' ','g'),
  '\s+',' ','g'))
$$;

-- The local date of an all-day event's start, in its zone (UTC when it has none or an unknown one).
CREATE FUNCTION same_event_day(t timestamp with time zone, tz text) RETURNS date
    LANGUAGE plpgsql STABLE
    AS $$ BEGIN
 RETURN (t AT TIME ZONE coalesce(nullif(tz,''),'UTC'))::date;
EXCEPTION WHEN invalid_parameter_value THEN
 RETURN (t AT TIME ZONE 'UTC')::date;
END $$;

-- The hint on a unit's card: up to `n` units that are possibly the same event, best first, each
-- as {id, uri, host, site, start, score} (the API derives the card's site from these). NULL for a
-- card that is no event. A member of the card's own group is never listed. Read at request time.
CREATE FUNCTION same_event_hint(unit text, type_path text[], n integer) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT CASE WHEN coalesce(type_path @> ARRAY['Event'],false) THEN coalesce((
  SELECT jsonb_agg(jsonb_build_object('id',x.id,'uri',x.uri,'host',x.host,'site',x.site,'start',x.start,'score',x.score) ORDER BY x.score DESC,x.start,x.uri)
  FROM (SELECT r.id,coalesce(nullif(o.url,''),r.uri) AS uri,r.host,r.doc->>'site' AS site,coalesce(o.start_at,r.start_at) AS start,round(p.score::numeric,2) AS score
   FROM same_event p JOIN same_event_unit u ON u.unit_id=p.other_id JOIN resource r ON r.id=u.resource_id
   LEFT JOIN occurrence o ON u.uid IS NOT NULL AND o.resource_id=u.resource_id AND o.uid=u.uid AND o.recurrence_id=u.recurrence_id
   WHERE p.unit_id=unit AND (u.uid IS NULL OR o.uid IS NOT NULL)
   AND NOT EXISTS (SELECT 1 FROM group_unit a JOIN group_unit b ON b.group_id=a.group_id WHERE a.unit_id=unit AND b.unit_id=p.other_id AND a.group_id IS NOT NULL)
   ORDER BY p.score DESC,coalesce(o.start_at,r.start_at),r.id LIMIT n) x),'[]'::jsonb) END
$$;
