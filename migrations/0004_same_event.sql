-- The same-event hint: event units (an event resource, or one occurrence of a feed) on different
-- pages that are possibly the same event. A hint only: never a group, never counted, never ranks.
-- A background pass (same_event::drain) writes the pairs; cards only read them.

-- A unit the pass has read, with what pairs are matched on: its start, normalised name
-- (`same_event_name`) and the numbers it keeps, place, venue (resource or OSM object), organizer
-- URL and zone; and the `fingerprint` of the values they came from, so a change is read again.
-- Pairs are matched in this table, through its start and geography indexes.
CREATE TABLE same_event_unit (
    unit_id text PRIMARY KEY,
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    uid text,
    recurrence_id text,
    fingerprint text NOT NULL,
    start_at timestamp with time zone NOT NULL,
    name_key text NOT NULL,
    numbers text[] NOT NULL,
    point geometry(Point,4326),
    venue_id text,
    osm_venue text,
    organizer text,
    all_day boolean NOT NULL,
    tz text
);
CREATE INDEX same_event_unit_resource_idx ON same_event_unit USING btree (resource_id);
CREATE INDEX same_event_unit_start_idx ON same_event_unit USING btree (start_at);
CREATE INDEX same_event_unit_geography_idx ON same_event_unit USING gist (((point)::geography));

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

-- Changes that can move a pair: an event resource's start, place, name or document (a former
-- event too), and a feed's occurrences. The pass unmarks a resource only once no save holding
-- its row is in flight (the conflict arm locks the row).
CREATE FUNCTION mark_same_event_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE target text;
BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF NOT coalesce(NEW.type_path @> ARRAY['Event'],false)
     AND NOT (TG_OP='UPDATE' AND coalesce(OLD.type_path @> ARRAY['Event'],false)) THEN RETURN NULL; END IF;
  IF TG_OP='UPDATE' AND (NEW.name,NEW.start_at,NEW.point,NEW.venue_id,NEW.type_path) IS NOT DISTINCT FROM (OLD.name,OLD.start_at,OLD.point,OLD.venue_id,OLD.type_path) THEN
   IF NEW.doc-'fetched_at'-'ttl_secs'-'warnings' IS NOT DISTINCT FROM OLD.doc-'fetched_at'-'ttl_secs'-'warnings' THEN RETURN NULL; END IF;
  END IF;
  target:=NEW.id;
 ELSIF TG_OP='DELETE' THEN
  IF NOT EXISTS (SELECT 1 FROM resource WHERE id=OLD.resource_id) THEN RETURN NULL; END IF;
  target:=OLD.resource_id;
 ELSE
  IF TG_OP='UPDATE' AND (NEW.name,NEW.start_at,NEW.point,NEW.venue_id,NEW.status,NEW.url) IS NOT DISTINCT FROM (OLD.name,OLD.start_at,OLD.point,OLD.venue_id,OLD.status,OLD.url) THEN
   IF NEW.doc IS NOT DISTINCT FROM OLD.doc THEN RETURN NULL; END IF;
  END IF;
  target:=NEW.resource_id;
 END IF;
 INSERT INTO same_event_dirty(resource_id) VALUES(target)
  ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at WHERE same_event_dirty.next_attempt_at>excluded.next_attempt_at;
 RETURN NULL;
END $$;

CREATE TRIGGER same_event_resource_dirty AFTER INSERT OR UPDATE OF name, start_at, point, venue_id, type_path, doc ON resource FOR EACH ROW EXECUTE FUNCTION mark_same_event_dirty();
CREATE TRIGGER same_event_occurrence_dirty AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION mark_same_event_dirty();

-- Bumped whenever the pass or the sweep changes pairs: search pages hash it into their ETag.
CREATE SEQUENCE same_event_revision;

-- An event name as the hint compares it: lower case; without dates, times and years
-- (`21.10.2026`, `2026-10-21`, `6.11.`, `21.`, `19:00`, `7pm`, `2026`, a month name with its
-- day: `Oct 21`, `6th November`); without punctuation, symbols and emoji; without weekday names
-- and filler words such as "meetup" and "event". Other numbers stay (`#50`, `Nr. 30`, `Room 101`).
CREATE FUNCTION same_event_name(name text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT btrim(regexp_replace(regexp_replace(regexp_replace(regexp_replace(regexp_replace(regexp_replace(lower(coalesce(name,'')),
  '\m(\d{4}-\d{1,2}-\d{1,2}|\d{1,2}[./]\d{1,2}([./]\d{2,4})?\.?|\d{1,2}[:h]\d{2}(h|uhr)?|\d{1,2} ?(am|pm|uhr)\M|(19|20)\d{2}\M|\d{1,2}\.(?!\d))',' ','g'),
  '\m(\d{1,2}(st|nd|rd|th)? ?)?(jan|january|januar|janvier|gennaio|feb|february|februar|fevrier|février|febbraio|mar|march|maerz|märz|marz|mars|marzo|apr|april|avril|aprile|may|mai|maggio|jun|june|juni|juin|giugno|jul|july|juli|juillet|luglio|aug|august|aout|août|agosto|sep|sept|september|septembre|settembre|oct|october|okt|oktober|octobre|ottobre|nov|november|novembre|dec|december|dez|dezember|decembre|décembre|dicembre)\M( ?\d{1,2}(st|nd|rd|th)?\M)?',' ','g'),
  '[!-/:-@[-`{-~ -¿×÷ -⯿　-〿︀-️\U0001f000-\U0001faff]+',' ','g'),
  '\m(meetups?|meet ?ups?|events?|edition|session|monthly|weekly|stammtisch|treffen|rencontre|incontro|jan|january|januar|janvier|gennaio|feb|february|februar|fevrier|février|febbraio|mar|march|maerz|märz|marz|mars|marzo|apr|april|avril|aprile|may|mai|maggio|jun|june|juni|juin|giugno|jul|july|juli|juillet|luglio|aug|august|aout|août|agosto|sep|sept|september|septembre|settembre|oct|october|okt|oktober|octobre|ottobre|nov|november|novembre|dec|december|dez|dezember|decembre|décembre|dicembre|mon|monday|montag|lundi|tue|tuesday|dienstag|mardi|wed|wednesday|mittwoch|mercredi|thu|thursday|donnerstag|jeudi|fri|friday|freitag|vendredi|sat|saturday|samstag|samedi|sun|sunday|sonntag|dimanche)\M',' ','g'),
  '\s+',' ','g'),
  '^ | $','','g'))
$$;

-- The numbered words a normalised name keeps (`50` of "Meetup #50"), sorted: two names that both
-- keep numbers are the same event only with the same numbers.
CREATE FUNCTION same_event_numbers(name text) RETURNS text[]
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT coalesce(array_agg(DISTINCT m[1] ORDER BY m[1]),'{}') FROM regexp_matches(same_event_name(name),'(\S*[0-9]\S*)','g') m
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
-- card that is no event. A member of the card's own group, an alias and an occurrence that is
-- gone are never listed. Read at request time.
CREATE FUNCTION same_event_hint(unit text, type_path text[], n integer) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT CASE WHEN coalesce(type_path @> ARRAY['Event'],false) THEN coalesce((
  SELECT jsonb_agg(jsonb_build_object('id',x.id,'uri',x.uri,'host',x.host,'site',x.site,'start',x.start,'score',x.score) ORDER BY x.score DESC,x.start,x.uri)
  FROM (SELECT r.id,coalesce(nullif(o.url,''),r.uri) AS uri,r.host,r.doc->>'site' AS site,coalesce(o.start_at,r.start_at) AS start,round(p.score::numeric,2) AS score
   FROM same_event p JOIN same_event_unit u ON u.unit_id=p.other_id JOIN resource r ON r.id=u.resource_id
   LEFT JOIN occurrence o ON u.uid IS NOT NULL AND o.resource_id=u.resource_id AND o.uid=u.uid AND o.recurrence_id=u.recurrence_id
   WHERE p.unit_id=unit AND (u.uid IS NULL OR (o.uid IS NOT NULL AND r.status<>'missing'))
   AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id=u.resource_id)
   AND NOT EXISTS (SELECT 1 FROM group_unit a JOIN group_unit b ON b.group_id=a.group_id WHERE a.unit_id=unit AND b.unit_id=p.other_id AND a.group_id IS NOT NULL)
   ORDER BY p.score DESC,coalesce(o.start_at,r.start_at),r.id LIMIT n) x),'[]'::jsonb) END
$$;

-- The events already stored that start in the window: the pass reads them first.
INSERT INTO same_event_dirty(resource_id)
SELECT DISTINCT s.id FROM search_unit s
WHERE s.type_path @> ARRAY['Event'] AND s.name IS NOT NULL AND s.start_at BETWEEN now()-interval '1 day' AND now()+interval '1 year'
ON CONFLICT DO NOTHING;
