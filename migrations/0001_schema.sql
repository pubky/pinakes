-- The Pinakes schema. Until the first deployment this is the only file: a schema change edits it,
-- and local and test databases are recreated (docs/operations.md, "Schema").
--
-- In order: extensions; resources and their references; events and OpenStreetMap places; the
-- read views; the search revision; groups; also-on; the same-event hint; the rows the code
-- updates in place.

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS h3;

-- Functions may name tables and views defined further down.
SET LOCAL check_function_bodies = false;


-- ── Resources ────────────────────────────────────────────────────────────────────────────────

-- An event document whose location came from a geocoder or OSM holds the OSM reference only, not
-- the derived place: OSM-derived values never enter the resource or change tables.
CREATE FUNCTION event_boundary(d jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT coalesce(d#>>'{venue,location_source}' NOT IN ('geocoded','osm'),true)
 OR (NOT d ? 'location' AND NOT (d->'venue') ? 'location')
$$;

-- One URI that someone referenced: its document (`doc`, the card), the facts a query needs
-- beside it, and its place in the resolve queue. An OSM object holds only its reference here;
-- its place is in `osm_place`.
CREATE TABLE resource (
    id text PRIMARY KEY CONSTRAINT resource_id_check CHECK (id ~ '^[0-9a-f]{32}$'),
    uri text NOT NULL UNIQUE,
    host text,
    type text,
    type_path text[] DEFAULT '{}',
    name text,
    category text,
    point geometry(Point,4326),
    start_at timestamp with time zone,
    end_at timestamp with time zone,
    venue_id text,
    doc jsonb NOT NULL DEFAULT '{}' CONSTRAINT resource_doc_check CHECK (octet_length(doc::text) <= 8192),
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(doc->>'description', ''))) STORED,
    resolver text,
    resolved_at timestamp with time zone,
    refresh_at timestamp with time zone,
    status text NOT NULL DEFAULT 'pending' CONSTRAINT resource_status_check CHECK (status IN ('ok', 'pending', 'missing', 'unreachable', 'error')),
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    -- Set while nothing references the resource; it is dropped after seven days.
    orphaned_at timestamp with time zone DEFAULT now(),
    -- The resolve queue.
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamp with time zone NOT NULL DEFAULT now(),
    lease_until timestamp with time zone,
    generation bigint NOT NULL DEFAULT 0,
    dead_letter boolean NOT NULL DEFAULT false,
    last_error text,
    osm_type text,
    osm_id bigint,
    final boolean NOT NULL DEFAULT false,
    missing_streak integer NOT NULL DEFAULT 0,
    last_missing_at timestamp with time zone,
    priority_at timestamp with time zone,
    -- The `osm_place.versions` entry this resource shows.
    osm_version text,
    ever_existed boolean NOT NULL DEFAULT false,
    timeout_streak integer NOT NULL DEFAULT 0 CONSTRAINT resource_timeout_streak_check CHECK (timeout_streak >= 0),
    denial_streak integer NOT NULL DEFAULT 0 CONSTRAINT resource_denial_streak_check CHECK (denial_streak >= 0),
    denied_since timestamp with time zone,
    -- Bumped by any change grouping reads (groups::fence).
    group_revision bigint NOT NULL DEFAULT 0,
    CONSTRAINT resource_event_boundary CHECK (event_boundary(doc) AND (coalesce(doc #>> '{venue,location_source}' NOT IN ('geocoded', 'osm'), true) OR point IS NULL)),
    CONSTRAINT resource_event_status CHECK (doc->>'event_status' IS NULL OR doc->>'event_status' IN ('scheduled', 'cancelled', 'postponed', 'rescheduled', 'moved_online')),
    CONSTRAINT resource_osm_separation CHECK ((osm_id IS NULL AND osm_type IS NULL) OR (osm_id IS NOT NULL AND osm_type IS NOT NULL AND osm_id > 0 AND osm_type IN ('node', 'way', 'relation') AND resolver IS NOT DISTINCT FROM 'osm' AND name IS NULL AND point IS NULL AND category IS NULL AND type IS NULL AND type_path IS NULL AND doc->>'type' IS NOT DISTINCT FROM 'Place' AND doc - ARRAY['type', 'url', 'osm_id', 'source', 'status'] = '{}')),
    CONSTRAINT resource_web_osm_boundary CHECK (resolver IS DISTINCT FROM 'web' OR lower(rtrim(host, '.')) !~ '(^|[.])(openstreetmap[.]org|osm[.]org)$' OR (name IS NULL AND point IS NULL AND category IS NULL AND type IS NULL AND doc = '{}')),
    -- Every instant taken from a source (event and reference times, a feed's horizon, a derived
    -- link's expiry) lies in UTC years 0001–9999, the range Postgres reads back through its text
    -- form; the code bounds them before saving. Other timestamps come from the server clock.
    CONSTRAINT resource_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT resource_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00')
);
CREATE INDEX resource_category_idx ON resource USING btree (category);
CREATE INDEX resource_claim_priority_idx ON resource USING btree (resolver, priority_at DESC NULLS LAST, next_attempt_at) WHERE NOT dead_letter AND NOT final;
CREATE INDEX resource_geography_idx ON resource USING gist ((point::geography));
CREATE INDEX resource_host_idx ON resource USING btree (host);
CREATE INDEX resource_name_trgm_idx ON resource USING gin (name gin_trgm_ops);
CREATE INDEX resource_orphaned_idx ON resource USING btree (orphaned_at) WHERE orphaned_at IS NOT NULL;
CREATE INDEX resource_osm_idx ON resource USING btree (osm_type, osm_id) WHERE osm_id IS NOT NULL;
CREATE INDEX resource_point_idx ON resource USING gist (point);
CREATE INDEX resource_queue_idx ON resource USING btree (resolver, next_attempt_at) WHERE NOT dead_letter;
CREATE INDEX resource_search_idx ON resource USING gin (search_text);
CREATE INDEX resource_start_idx ON resource USING btree (start_at);
CREATE INDEX resource_type_path_idx ON resource USING gin (type_path);
CREATE INDEX resource_venue_idx ON resource USING btree (venue_id);
CREATE INDEX resource_venue_version_idx ON resource USING btree ((doc #>> '{venue,osm_id}'), (doc #>> '{venue,osm_version}'));

-- A resource whose last error is not a denial starts its denial streak over.
CREATE FUNCTION reset_denial_streak() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF NEW.last_error IS NULL OR NEW.last_error NOT IN('upstream_denied','graphql') THEN
  NEW.denial_streak=0; NEW.denied_since=NULL;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER reset_denial_streak BEFORE UPDATE OF last_error ON resource FOR EACH ROW EXECUTE FUNCTION reset_denial_streak();

-- The same object under another URI (alias::root, alias::canonical): never fetched, its
-- references count for the root and reads answer with the root. A music service's and
-- Wikipedia's URL forms are spellings of one object (`service`).
CREATE TABLE resource_alias (
    alias_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    root_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    basis text NOT NULL,
    CONSTRAINT resource_alias_check CHECK (alias_id <> root_id),
    CONSTRAINT resource_alias_basis CHECK (basis IN ('cursor', 'osm', 'identifier', 'service', 'tracking', 'canonical', 'source'))
);
CREATE INDEX resource_alias_root ON resource_alias USING btree (root_id);

-- One reference from the Nexus feed (a tag, reply, embed, collection entry, event, or a post
-- naming itself), keyed by the object that makes it; an event reference carries its own times
-- and status.
CREATE TABLE reference (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    kind text NOT NULL CONSTRAINT reference_kind_check CHECK (kind IN ('tag', 'reply', 'embed', 'collection', 'event', 'post')),
    author text NOT NULL,
    label text,
    object_path text NOT NULL,
    recurrence_id text,
    start_at timestamp with time zone,
    end_at timestamp with time zone,
    created_at timestamp with time zone NOT NULL,
    unit_key text NOT NULL DEFAULT '',
    event_status text,
    PRIMARY KEY (author, object_path, unit_key),
    CONSTRAINT reference_event_status CHECK (event_status IS NULL OR event_status IN ('scheduled', 'cancelled', 'postponed', 'rescheduled', 'moved_online')),
    CONSTRAINT reference_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT reference_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT reference_created_at_range CHECK (created_at >= '0001-01-01 00:00:00+00' AND created_at < '10000-01-01 00:00:00+00')
);
-- Search's `related` filter reads each tagger's most recent references in its window; the
-- index serves every lookup by author too.
CREATE INDEX reference_author_created_idx ON reference USING btree (author, created_at);
-- `sort=trending` counts the references made inside a window.
CREATE INDEX reference_created_idx ON reference USING btree (created_at);
CREATE INDEX reference_event_idx ON reference USING btree (resource_id, start_at) WHERE kind = 'event';
CREATE INDEX reference_label_idx ON reference USING btree (label, resource_id);
-- A resource's most recent references (`related` reads its tags in a window); serves every
-- lookup by resource too.
CREATE INDEX reference_resource_created_idx ON reference USING btree (resource_id, created_at);
-- `related`'s taggers are the authors of a resource's most recent tags: an index of its
-- tags alone, so a resource with many replies or embeds and few tags reads only tags.
CREATE INDEX reference_resource_tag_idx ON reference USING btree (resource_id, created_at) WHERE kind = 'tag';
CREATE INDEX reference_start_idx ON reference USING btree (start_at);

-- References as they count: an alias's count for its root, posts' own rows left out.
CREATE VIEW effective_reference AS
    SELECT coalesce(a.root_id, f.resource_id) AS resource_id, f.author, f.object_path, f.kind, f.label, f.created_at
    FROM reference f LEFT JOIN resource_alias a ON a.alias_id = f.resource_id
    -- A post's own row keeps its card alive; it references nothing else, so it does not count.
    WHERE f.kind <> 'post';

-- Resources a page derives (an event adapter's entries), each with its adapter and when the
-- link lapses.
CREATE TABLE resource_derivation (
    parent_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    child_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    adapter text NOT NULL,
    expires_at timestamp with time zone,
    until timestamp with time zone,
    first_seen_at timestamp with time zone NOT NULL DEFAULT now(),
    -- Feed row key -> the instant that row asks for (store::link_sources).
    sources jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (parent_id, child_id),
    CONSTRAINT resource_derivation_check CHECK (parent_id <> child_id),
    CONSTRAINT resource_derivation_expires_at_range CHECK (expires_at >= '0001-01-01 00:00:00+00' AND expires_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT resource_derivation_until_range CHECK (until >= '0001-01-01 00:00:00+00' AND until < '10000-01-01 00:00:00+00')
);
CREATE INDEX resource_derivation_adapter_expiry ON resource_derivation USING btree (adapter, expires_at);
CREATE INDEX resource_derivation_child ON resource_derivation USING btree (child_id);

-- A resource's score from its references (tags count most), for ranking.
CREATE TABLE resource_score (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    score double precision NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    newest_at timestamp with time zone NOT NULL
);

-- Where the Nexus feed was read up to, per Nexus.
CREATE TABLE feed_cursor (
    nexus text PRIMARY KEY,
    cursor bigint NOT NULL DEFAULT 0 CONSTRAINT feed_cursor_cursor_check CHECK (cursor >= 0),
    epoch text,
    feed_length bigint NOT NULL DEFAULT 0 CONSTRAINT feed_cursor_feed_length_check CHECK (feed_length >= 0),
    updated_at timestamp with time zone NOT NULL DEFAULT now(),
    resets bigint NOT NULL DEFAULT 0
);

-- Per upstream source: last success and error, change and hold totals, and its canary.
CREATE TABLE source_state (
    source text PRIMARY KEY,
    last_success_at timestamp with time zone,
    last_error text,
    consecutive_failures integer NOT NULL DEFAULT 0,
    change_totals jsonb NOT NULL DEFAULT '{}',
    feed_held_total bigint NOT NULL DEFAULT 0,
    cursor_resets_total bigint NOT NULL DEFAULT 0,
    canary_at timestamp with time zone,
    canary_ok boolean,
    canary_missing text[] NOT NULL DEFAULT '{}',
    canary_failures integer NOT NULL DEFAULT 0,
    canary_due_at timestamp with time zone,
    canary_lease_until timestamp with time zone,
    canary_generation bigint NOT NULL DEFAULT 0,
    CONSTRAINT source_error_class CHECK (last_error IS NULL OR last_error IN ('dns', 'connect', 'timeout', 'http_4xx', 'http_5xx', 'parse', 'policy'))
);


-- ── Events and places ────────────────────────────────────────────────────────────────────────

-- An OpenStreetMap object as Pinakes read it, with each version a resource or event refers to
-- (`versions`). The only table that holds OSM-derived values.
CREATE TABLE osm_place (
    osm_type text NOT NULL CONSTRAINT osm_place_osm_type_check CHECK (osm_type IN ('node', 'way', 'relation')),
    osm_id bigint NOT NULL CONSTRAINT osm_place_osm_id_check CHECK (osm_id > 0),
    point geometry(Point,4326),
    bbox double precision[],
    name text,
    category text,
    address jsonb NOT NULL DEFAULT '{}',
    tags jsonb NOT NULL DEFAULT '{}',
    display_name text,
    type text NOT NULL,
    type_path text[] NOT NULL,
    doc jsonb NOT NULL CONSTRAINT osm_place_doc_check CHECK (octet_length(doc::text) <= 8192),
    fetched_at timestamp with time zone NOT NULL,
    refresh_at timestamp with time zone NOT NULL,
    status text NOT NULL CONSTRAINT osm_place_status_check CHECK (status IN ('ok', 'missing')),
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(display_name, ''))) STORED,
    fetch_doc jsonb NOT NULL,
    fetch_display_name text,
    versions jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (osm_type, osm_id)
);
COMMENT ON TABLE osm_place IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';
CREATE INDEX osm_place_category_idx ON osm_place USING btree (category);
CREATE INDEX osm_place_geography_idx ON osm_place USING gist ((point::geography));
CREATE INDEX osm_place_name_idx ON osm_place USING gin (name gin_trgm_ops);
CREATE INDEX osm_place_point_idx ON osm_place USING gist (point);
CREATE INDEX osm_place_search_idx ON osm_place USING gin (search_text);
CREATE INDEX osm_place_type_path_idx ON osm_place USING gin (type_path);

-- An event document with its OSM venue's location filled in from `osm_place`, at read time.
CREATE FUNCTION event_document(d jsonb) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $_$
 SELECT CASE WHEN d#>>'{venue,osm_id}' IS NULL THEN d ELSE coalesce((
 SELECT CASE WHEN p IS NULL THEN d ELSE d || jsonb_build_object('location',p->'location','location_source',d#>'{venue,location_source}','attribution','© OpenStreetMap contributors (ODbL)','venue',(d->'venue')||jsonb_build_object('location',p->'location')) END
 FROM osm_place o CROSS JOIN LATERAL (SELECT o.versions->(d#>>'{venue,osm_version}') AS p) v
 WHERE o.osm_type=split_part(d#>>'{venue,osm_id}','/',1)
 AND o.osm_id=CASE WHEN split_part(d#>>'{venue,osm_id}','/',2) ~ '^[0-9]{1,18}$' THEN split_part(d#>>'{venue,osm_id}','/',2)::bigint END
 ),d) END
$_$;

-- One occurrence of a calendar feed (a resource), by its iCalendar UID and recurrence ID.
CREATE TABLE occurrence (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    uid text NOT NULL,
    recurrence_id text NOT NULL DEFAULT '',
    start_at timestamp with time zone NOT NULL,
    end_at timestamp with time zone,
    name text,
    venue_id text,
    status text NOT NULL,
    url text,
    doc jsonb NOT NULL DEFAULT '{}',
    point geometry(Point,4326),
    sequence bigint NOT NULL DEFAULT 0,
    modified timestamp with time zone,
    content_hash text NOT NULL DEFAULT '',
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(doc->>'description', ''))) STORED,
    PRIMARY KEY (resource_id, uid, recurrence_id),
    CONSTRAINT occurrence_document_cap CHECK (octet_length(doc::text) <= 8192),
    CONSTRAINT occurrence_event_boundary CHECK (event_boundary(doc) AND (coalesce(doc #>> '{venue,location_source}' NOT IN ('geocoded', 'osm'), true) OR point IS NULL)),
    CONSTRAINT occurrence_status CHECK (status IN ('scheduled', 'cancelled', 'postponed', 'rescheduled', 'moved_online', 'superseded')),
    CONSTRAINT occurrence_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT occurrence_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT occurrence_modified_range CHECK (modified >= '0001-01-01 00:00:00+00' AND modified < '10000-01-01 00:00:00+00')
);
CREATE INDEX occurrence_geography_idx ON occurrence USING gist ((point::geography));
CREATE INDEX occurrence_hash_idx ON occurrence USING btree (resource_id, content_hash, start_at, name);
CREATE INDEX occurrence_page_idx ON occurrence USING btree (resource_id, start_at, uid, recurrence_id);
CREATE INDEX occurrence_point_idx ON occurrence USING gist (point);
CREATE INDEX occurrence_retention_idx ON occurrence USING btree (end_at, start_at);
CREATE INDEX occurrence_search_idx ON occurrence USING gin (search_text);
CREATE INDEX occurrence_start_idx ON occurrence USING btree (start_at);
CREATE INDEX occurrence_venue_idx ON occurrence USING btree (venue_id);
CREATE INDEX occurrence_venue_version_idx ON occurrence USING btree ((doc #>> '{venue,osm_id}'), (doc #>> '{venue,osm_version}'));

-- How a calendar feed was last fetched: validators, snapshot, held and missing UIDs, retention
-- and horizon, and an adapter's cursor.
CREATE TABLE event_fetch (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    validators jsonb NOT NULL DEFAULT '{}',
    last_snapshot_at bigint,
    held_keys text[],
    missing_keys jsonb NOT NULL DEFAULT '{}',
    hold_count integer NOT NULL DEFAULT 0,
    retention_days integer NOT NULL DEFAULT 30 CONSTRAINT event_fetch_retention_days_check CHECK (retention_days >= 1 AND retention_days <= 3650),
    horizon_start timestamp with time zone,
    horizon_end timestamp with time zone,
    adapter_cursor text CONSTRAINT event_fetch_adapter_cursor_check CHECK (octet_length(adapter_cursor) <= 4096),
    adapter_resync boolean NOT NULL DEFAULT false,
    CONSTRAINT event_fetch_horizon_start_range CHECK (horizon_start >= '0001-01-01 00:00:00+00' AND horizon_start < '10000-01-01 00:00:00+00'),
    CONSTRAINT event_fetch_horizon_end_range CHECK (horizon_end >= '0001-01-01 00:00:00+00' AND horizon_end < '10000-01-01 00:00:00+00')
);

-- Occurrences as they are read: superseded ones left out, an OSM venue's location filled in.
CREATE VIEW event_occurrence AS
    SELECT o.*, o.doc AS document, o.point AS effective_point
    FROM occurrence o
    WHERE o.doc #>> '{venue,osm_id}' IS NULL AND o.status <> 'superseded'
UNION ALL
    SELECT x.*, event_document(x.doc) AS document, o.point AS effective_point
    FROM occurrence x
    LEFT JOIN osm_place o ON o.osm_type = split_part(x.doc #>> '{venue,osm_id}', '/', 1)
        AND o.osm_id = CASE WHEN split_part(x.doc #>> '{venue,osm_id}', '/', 2) ~ '^[0-9]{1,18}$' THEN split_part(x.doc #>> '{venue,osm_id}', '/', 2)::bigint END
    WHERE x.doc #>> '{venue,osm_id}' IS NOT NULL AND x.status <> 'superseded';

-- The change journal (`changes`): one row per change to a resource or occurrence that a reader
-- sees. An OSM change keeps only the OSM reference and its versions.
CREATE TABLE resource_change (
    seq bigint NOT NULL,
    resource_id text NOT NULL,
    occurrence_key text,
    at timestamp with time zone NOT NULL DEFAULT clock_timestamp(),
    class text NOT NULL CONSTRAINT resource_change_class_check CHECK (class IN ('created', 'rescheduled', 'relocated', 'cancelled', 'uncancelled', 'renamed', 'details', 'deleted', 'aliased')),
    fields text[] NOT NULL,
    before jsonb,
    after jsonb,
    source text NOT NULL,
    osm_type text,
    osm_id bigint,
    CONSTRAINT change_event_boundary CHECK (event_boundary(before) AND event_boundary(after)),
    CONSTRAINT change_web_osm_boundary CHECK (source IN ('osm', 'geo') OR (coalesce(before->>'url', '') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)' AND coalesce(after->>'url', '') !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)')),
    CONSTRAINT resource_change_check CHECK (source <> 'osm' OR (osm_type IS NOT NULL AND osm_id IS NOT NULL AND (before IS NULL OR before - ARRAY['osm_id', 'version'] = '{}') AND (after IS NULL OR after - ARRAY['osm_id', 'version'] = '{}')))
);
CREATE SEQUENCE resource_change_seq_seq START WITH 1 INCREMENT BY 1 NO MINVALUE NO MAXVALUE CACHE 1 OWNED BY resource_change.seq;
ALTER TABLE resource_change ALTER COLUMN seq SET DEFAULT nextval('resource_change_seq_seq');
ALTER TABLE resource_change ADD PRIMARY KEY (seq);
CREATE INDEX resource_change_after_venue_idx ON resource_change USING btree ((after #>> '{venue,osm_id}'), (after #>> '{venue,osm_version}'));
CREATE INDEX resource_change_at_idx ON resource_change USING btree (at);
CREATE INDEX resource_change_before_venue_idx ON resource_change USING btree ((before #>> '{venue,osm_id}'), (before #>> '{venue,osm_version}'));
CREATE INDEX resource_change_osm_idx ON resource_change USING btree (osm_type, osm_id) WHERE osm_type IS NOT NULL;
CREATE INDEX resource_change_resource_idx ON resource_change USING btree (resource_id, seq);

-- Changes as they are read: an OSM change shows the place versions it names, an event change its
-- venue's location.
CREATE VIEW change_document AS
    SELECT c.seq, c.resource_id, c.occurrence_key, c.at, c.class, c.fields, c.source,
        CASE WHEN c.source = 'osm' AND c.before IS NOT NULL THEN o.versions -> (c.before->>'version') ELSE event_document(c.before) END AS before,
        CASE WHEN c.source = 'osm' AND c.after IS NOT NULL THEN o.versions -> (c.after->>'version') ELSE event_document(c.after) END AS after,
        CASE WHEN c.source = 'osm' OR c.before #>> '{venue,osm_id}' IS NOT NULL OR c.after #>> '{venue,osm_id}' IS NOT NULL
            THEN '© OpenStreetMap contributors (ODbL)' END AS attribution
    FROM resource_change c LEFT JOIN osm_place o ON o.osm_type = c.osm_type AND o.osm_id = c.osm_id;


-- ── Documents as they are read ───────────────────────────────────────────────────────────────

-- Every resource's document, with OSM-derived values joined in at read time: as stored; an event
-- with an OSM venue; an OSM object, from the `osm_place` version it shows.
CREATE VIEW resource_document AS
    SELECT r.id, r.uri, r.host, r.type, r.type_path, r.name, r.category, r.point, r.start_at, r.end_at, r.venue_id,
        r.doc, r.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id
    FROM resource r
    WHERE r.osm_id IS NULL AND r.doc #>> '{venue,osm_id}' IS NULL
UNION ALL
    SELECT r.id, r.uri, r.host, r.type, r.type_path, r.name, r.category, o.point, r.start_at, r.end_at, r.venue_id,
        event_document(r.doc) AS doc, r.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id
    FROM resource r
    LEFT JOIN osm_place o ON o.osm_type = split_part(r.doc #>> '{venue,osm_id}', '/', 1)
        AND o.osm_id = CASE WHEN split_part(r.doc #>> '{venue,osm_id}', '/', 2) ~ '^[0-9]{1,18}$' THEN split_part(r.doc #>> '{venue,osm_id}', '/', 2)::bigint END
    WHERE r.osm_id IS NULL AND r.doc #>> '{venue,osm_id}' IS NOT NULL
UNION ALL
    SELECT r.id, r.uri, r.host, o.type, o.type_path, o.name, o.category, o.point, r.start_at, r.end_at, r.venue_id,
        coalesce(o.versions -> r.osm_version || jsonb_build_object('url', r.uri, 'fetched_at', (extract(epoch FROM r.resolved_at) * 1000)::bigint, 'ttl_secs', greatest(0::bigint, extract(epoch FROM r.refresh_at - r.resolved_at)::bigint)), '{}') AS doc,
        o.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id
    FROM resource r
    LEFT JOIN osm_place o ON o.osm_type = r.osm_type AND o.osm_id = r.osm_id AND r.osm_version IS NOT NULL
    WHERE r.osm_id IS NOT NULL;

-- The units search ranks: each resource that is not an alias (an event only with a start), each
-- live occurrence of a feed that is not missing, and each event reference with a start.
CREATE VIEW search_unit AS
    SELECT r.*, r.id AS unit_id, NULL::jsonb AS occurrence, NULL::jsonb AS event, NULL::text AS event_author
    FROM resource_document r
    WHERE (NOT coalesce(r.type_path @> ARRAY['Event'], false) OR r.start_at IS NOT NULL)
        AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id = r.id)
UNION ALL
    SELECT r.id, r.uri, r.host, o.doc->>'type' AS type, ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')) AS type_path,
        o.name, NULL::text AS category, o.effective_point AS point, o.start_at, o.end_at, o.venue_id, o.document AS doc,
        o.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, NULL::text AS osm_type, NULL::bigint AS osm_id,
        md5(jsonb_build_array(r.id, o.uid, o.recurrence_id)::text) AS unit_id,
        jsonb_build_object('uid', o.uid, 'recurrence_id', nullif(o.recurrence_id, ''), 'status', o.status) AS occurrence,
        NULL::jsonb AS event, NULL::text AS event_author
    FROM event_occurrence o JOIN resource r ON r.id = o.resource_id
    WHERE r.status <> 'missing'
UNION ALL
    SELECT r.id, r.uri, r.host, 'Event'::text AS type, ARRAY['Thing', 'Event'] AS type_path, NULL::text AS name, NULL::text AS category,
        r.point, f.start_at, f.end_at, r.id AS venue_id,
        jsonb_build_object('type', 'Event', 'type_path', jsonb_build_array('Thing', 'Event'), 'url', r.uri, 'site', coalesce(r.host, ''), 'source', 'nexus_reference', 'status', 'ok', 'attributes', '{}'::jsonb, 'start', f.start_at, 'end', f.end_at, 'event_status', coalesce(f.event_status, 'scheduled'))
            || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location', r.doc->'location') END AS doc,
        ''::tsvector AS search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id,
        md5(jsonb_build_array(r.id, f.author, f.object_path, f.recurrence_id)::text) AS unit_id,
        NULL::jsonb AS occurrence,
        jsonb_build_object('author', f.author, 'object_path', f.object_path, 'recurrence_id', f.recurrence_id, 'start', f.start_at, 'end', f.end_at, 'status', coalesce(f.event_status, 'scheduled')) AS event,
        f.author AS event_author
    FROM reference f
    LEFT JOIN resource_alias a ON a.alias_id = f.resource_id
    JOIN resource_document r ON r.id = coalesce(a.root_id, f.resource_id)
    WHERE f.kind = 'event' AND f.start_at IS NOT NULL;

-- A resource's own search unit: `search_unit`'s first branch, without occurrences and event
-- references, so `related` reads each related resource's card by its ID alone.
CREATE VIEW resource_unit AS
    SELECT r.*, r.id AS unit_id, NULL::jsonb AS occurrence, NULL::jsonb AS event, NULL::text AS event_author
    FROM resource_document r
    WHERE (NOT coalesce(r.type_path @> ARRAY['Event'], false) OR r.start_at IS NOT NULL)
        AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id = r.id);


-- ── The search revision ──────────────────────────────────────────────────────────────────────

-- Each transaction that changes searchable rows adds one row (a sequence value: no row is shared
-- between writers), and a reader's revision is the number of write transactions its snapshot
-- sees. A later snapshot sees every transaction an earlier one saw, so an equal revision means the
-- same writes, and a write still in flight is invisible together with its data. Search pages
-- hash it into their ETag.
CREATE SEQUENCE search_write_id_seq;
CREATE TABLE search_write (
    id bigint NOT NULL DEFAULT nextval('search_write_id_seq') PRIMARY KEY,
    -- How many write transactions the row counts (more than one after `compact_search_writes`).
    writes bigint NOT NULL DEFAULT 1
);
ALTER SEQUENCE search_write_id_seq OWNED BY search_write.id;

CREATE VIEW search_revision AS
    SELECT 1 AS id, coalesce(sum(writes), 0)::bigint AS revision FROM search_write;

CREATE FUNCTION bump_search_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    -- Once per transaction: the setting is transaction-local, and a rolled-back savepoint undoes
    -- it together with its row.
    IF current_setting('pinakes.search_write', true) IS DISTINCT FROM 'on' THEN
        INSERT INTO search_write DEFAULT VALUES;
        PERFORM set_config('pinakes.search_write', 'on', true);
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER resource_revision_insert BEFORE INSERT OR DELETE ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER resource_revision_update BEFORE UPDATE OF uri, host, resolver, doc, status, name, type, type_path, category, point, start_at, end_at, osm_version, resolved_at, refresh_at ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER reference_revision BEFORE INSERT OR DELETE OR UPDATE OR TRUNCATE ON reference FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER score_revision BEFORE INSERT OR DELETE OR UPDATE OR TRUNCATE ON resource_score FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER occurrence_revision BEFORE INSERT OR DELETE OR UPDATE ON occurrence FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER osm_revision_insert BEFORE INSERT OR DELETE ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER osm_revision_update BEFORE UPDATE OF doc, status, name, type, type_path, category, point, bbox ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();


-- ── Groups ───────────────────────────────────────────────────────────────────────────────────

-- A group: units that are the same thing (see docs/design.md). `curated` groups are kept when
-- they have no members.
CREATE TABLE "group" (
    id text PRIMARY KEY CONSTRAINT group_id_check CHECK (id ~ '^[0-9a-f]{32}$'),
    curated boolean NOT NULL DEFAULT false
);

-- A unit (a resource, an occurrence or an event reference) and its group, if any.
CREATE TABLE group_unit (
    unit_id text PRIMARY KEY CONSTRAINT group_unit_unit_id_check CHECK (unit_id ~ '^[0-9a-f]{32}$'),
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    occurrence_key text NOT NULL DEFAULT '',
    group_id text REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED,
    occurrence jsonb,
    event jsonb,
    admitted_at timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX group_unit_group_idx ON group_unit USING btree (group_id, resource_id, unit_id);
CREATE INDEX group_unit_resource_idx ON group_unit USING btree (resource_id);

-- The grouping keys (evidence) of each unit.
CREATE TABLE resource_group (
    unit_id text NOT NULL REFERENCES group_unit(unit_id) ON DELETE CASCADE,
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    group_id text REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED,
    key text NOT NULL CONSTRAINT resource_group_key_check CHECK (octet_length(key) <= 2200),
    admitted_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (unit_id, key)
);
CREATE INDEX resource_group_admission_idx ON resource_group USING btree (key, admitted_at, unit_id);
CREATE INDEX resource_group_group_idx ON resource_group USING btree (group_id);
CREATE INDEX resource_group_key_idx ON resource_group USING btree (key);
CREATE INDEX resource_group_resource_idx ON resource_group USING btree (resource_id);

-- A unit an operator placed in a curated group.
CREATE TABLE group_override (
    unit_id text PRIMARY KEY REFERENCES group_unit(unit_id) ON DELETE CASCADE,
    group_id text NOT NULL REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX group_override_group_idx ON group_override USING btree (group_id);

-- What grouping last read of a resource (`fingerprint`), so an unchanged one is skipped.
CREATE TABLE group_input (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    fingerprint text NOT NULL,
    parked_keys integer NOT NULL DEFAULT 0,
    settled_stopped boolean NOT NULL DEFAULT false,
    -- Joins the last run refused because they would put two values of one single-object hub key
    -- namespace (an MBID, a Q-id, a Discogs ID) in one group.
    key_conflicts integer NOT NULL DEFAULT 0
);

-- Resources grouping must read again, with its back-off.
CREATE TABLE group_dirty (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    attempts integer NOT NULL DEFAULT 0,
    last_error text,
    next_attempt_at timestamp with time zone NOT NULL DEFAULT now(),
    fence_attempts integer NOT NULL DEFAULT 0,
    resume_key text,
    stopped_fingerprint text,
    stopped_runs integer NOT NULL DEFAULT 0
);
CREATE INDEX group_dirty_due_idx ON group_dirty USING btree (next_attempt_at, resource_id);

-- The grouping policy last applied (one row): a change invalidates derived memberships.
CREATE TABLE group_runtime (
    id boolean PRIMARY KEY DEFAULT true CONSTRAINT group_runtime_id_check CHECK (id),
    settings jsonb
);

-- The hub key namespaces that name one object each, as every source that records them has
-- registered them (`groups::configure`, at start): a group holds at most one value of each. The
-- rule outlives the source: a removed source's keys stay until they expire, and group as before.
CREATE TABLE single_object_namespace (
    namespace text PRIMARY KEY,
    source text NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT single_object_namespace_namespace_check CHECK ((namespace ~ '^[a-z][a-z0-9_:-]{0,63}$')),
    CONSTRAINT single_object_namespace_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$'))
);

-- A group member's document, as its card shows it: a resource's, an occurrence's, or an event
-- reference's.
CREATE VIEW group_member_document AS
    SELECT r.*, u.unit_id, u.occurrence, u.event, NULL::text AS event_author
    FROM group_unit u
    CROSS JOIN LATERAL (SELECT * FROM resource_document WHERE resource_document.id = u.resource_id OFFSET 0) r
    WHERE u.occurrence IS NULL AND u.event IS NULL
UNION ALL
    SELECT r.id, r.uri, r.host, o.doc->>'type' AS type, ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')) AS type_path,
        o.name, NULL::text AS category, o.effective_point AS point, o.start_at, o.end_at, o.venue_id, o.document AS doc,
        o.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, NULL::text AS osm_type, NULL::bigint AS osm_id,
        u.unit_id, u.occurrence, NULL::jsonb AS event, NULL::text AS event_author
    FROM group_unit u
    CROSS JOIN LATERAL (SELECT * FROM event_occurrence
        WHERE event_occurrence.resource_id = u.resource_id AND event_occurrence.uid = u.occurrence->>'uid'
            AND event_occurrence.recurrence_id = coalesce(u.occurrence->>'recurrence_id', '') OFFSET 0) o
    JOIN resource r ON r.id = u.resource_id
    WHERE u.occurrence IS NOT NULL AND r.status <> 'missing'
UNION ALL
    SELECT r.id, r.uri, r.host, 'Event'::text AS type, ARRAY['Thing', 'Event'] AS type_path, NULL::text AS name, NULL::text AS category,
        r.point, f.start_at, f.end_at, r.id AS venue_id,
        jsonb_build_object('type', 'Event', 'type_path', jsonb_build_array('Thing', 'Event'), 'url', r.uri, 'site', coalesce(r.host, ''), 'source', 'nexus_reference', 'status', 'ok', 'attributes', '{}'::jsonb, 'start', f.start_at, 'end', f.end_at, 'event_status', coalesce(f.event_status, 'scheduled'))
            || CASE WHEN r.doc->'location' IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('location', r.doc->'location') END AS doc,
        ''::tsvector AS search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, r.osm_type, r.osm_id,
        u.unit_id, NULL::jsonb AS occurrence, u.event, f.author AS event_author
    FROM group_unit u
    JOIN reference f ON f.resource_id = u.resource_id AND f.author = u.event->>'author' AND f.object_path = u.event->>'object_path'
        AND f.recurrence_id = coalesce(u.event->>'recurrence_id', '')
    CROSS JOIN LATERAL (SELECT * FROM resource_document WHERE resource_document.id = u.resource_id OFFSET 0) r
    WHERE u.event IS NOT NULL AND f.kind = 'event' AND f.start_at IS NOT NULL;

-- A change grouping reads marks the resource dirty and bumps its `group_revision`; grouping's
-- fence (groups::fence) relies on the bump.
CREATE FUNCTION mark_group_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings') THEN RETURN NULL; END IF;
  INSERT INTO group_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  PERFORM 1 FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END FOR KEY SHARE;
  INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;
CREATE FUNCTION bump_group_resource_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 NEW.group_revision=OLD.group_revision+1;
 RETURN NEW;
END $$;
CREATE FUNCTION bump_group_child_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 UPDATE resource SET group_revision=group_revision+1
 WHERE id IN (CASE WHEN TG_OP<>'INSERT' THEN OLD.resource_id END, CASE WHEN TG_OP<>'DELETE' THEN NEW.resource_id END);
 RETURN NULL;
END $$;
-- Removing a unit from a group marks every member dirty, so it locks the members' rows first (in
-- ID order): a save of a member holds its row, then marks it dirty.
CREATE FUNCTION group_member_deleted() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 PERFORM 1 FROM resource WHERE id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id) ORDER BY id FOR KEY SHARE;
 DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id);
 INSERT INTO group_dirty(resource_id) SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id ON CONFLICT DO NOTHING;
 DELETE FROM "group" g WHERE id=OLD.group_id AND NOT curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id);
 RETURN NULL;
END $$;

-- The group hint on a card: the group's ID and size. The members it names, with the derived
-- links, are the card's `also_on` list (below).
CREATE FUNCTION group_hint(unit text) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT jsonb_build_object('id',u.group_id,'size',(SELECT count(*) FROM group_unit x WHERE x.group_id=u.group_id))
 FROM group_unit u WHERE u.unit_id=unit AND u.group_id IS NOT NULL
$$;

CREATE TRIGGER group_resource_dirty AFTER INSERT OR UPDATE OF doc ON resource FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
-- alias::assign's UPDATE of the alias names `doc` and `osm_version` so that this fires: keep those
-- columns in both places.
CREATE TRIGGER group_resource_revision BEFORE UPDATE OF doc, status, uri, host, point, start_at, end_at, type, name, osm_version ON resource FOR EACH ROW EXECUTE FUNCTION bump_group_resource_revision();
CREATE TRIGGER group_reference_dirty AFTER INSERT OR DELETE OR UPDATE ON reference FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER group_reference_revision AFTER INSERT OR DELETE OR UPDATE ON reference FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
CREATE TRIGGER group_occurrence_dirty AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER group_occurrence_revision AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
CREATE TRIGGER group_member_deleted AFTER DELETE ON group_unit FOR EACH ROW EXECUTE FUNCTION group_member_deleted();
CREATE TRIGGER group_units_revision BEFORE INSERT OR DELETE OR UPDATE ON group_unit FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER groups_revision BEFORE INSERT OR DELETE OR UPDATE ON "group" FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();


-- ── Also on ──────────────────────────────────────────────────────────────────────────────────

-- "Also on": the same thing on other services, as hints on each card (see docs/api.md).
--
-- A source (a hub such as MusicBrainz or Wikidata, or a service's own exact lookup) reads one
-- resource and returns join keys and derived links. Each run replaces that source's rows for
-- the resource (`source`); rows a source stops returning are deleted, and rows nobody refreshes
-- expire. `via` names the hub a row came through, as the card shows it.

-- Exact join keys a source vouches for, e.g. `mb:recording:{mbid}` or `wikidata:Q42`. Two
-- resources carrying the same key join a group (`hub:{key}` evidence), within the usual
-- distance, time and size guards. A page's own claims never become keys.
CREATE TABLE resource_key (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    key text NOT NULL,
    source text NOT NULL,
    via text NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    PRIMARY KEY (resource_id, key),
    CONSTRAINT resource_key_key_check CHECK ((octet_length(key) BETWEEN 3 AND 512)),
    CONSTRAINT resource_key_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$')),
    CONSTRAINT resource_key_via_check CHECK ((via ~ '^[a-z0-9_]{1,32}$'))
);
CREATE INDEX resource_key_source_idx ON resource_key USING btree (resource_id, source);
CREATE INDEX resource_key_key_idx ON resource_key USING btree (key);

-- Facts a source reads for a resource (`isrc`, `upc`, `mbid`, `artist`, `duration`): shown in
-- the read's `details.facts` and searchable by `key`; unlike keys they never join anything.
CREATE TABLE resource_fact (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    name text NOT NULL,
    value text NOT NULL,
    source text NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    PRIMARY KEY (resource_id, name, value),
    CONSTRAINT resource_fact_name_check CHECK ((name ~ '^[a-z][a-z0-9_]{0,31}$')),
    CONSTRAINT resource_fact_value_check CHECK ((octet_length(value) BETWEEN 1 AND 512)),
    CONSTRAINT resource_fact_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$'))
);
CREATE INDEX resource_fact_source_idx ON resource_fact USING btree (resource_id, source);
CREATE INDEX resource_fact_value_idx ON resource_fact USING btree (name, value);

-- Links to the same thing that nobody tagged, found by a source: shown in the card's
-- `also_on` with `derived: true`, never fetched, never counted as references, never ranked.
-- `link_id` is the link's resource ID, so a derived link and a tagged member that are the
-- same URL are shown once. `key` is the exact key that named the link (the hub entity or item it
-- is a relation or claim of, or a lookup by barcode or ISRC): a tagged resource with that ID
-- joins it (`groups::evidence`).
CREATE TABLE derived_link (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    link_id text NOT NULL,
    uri text NOT NULL,
    site text NOT NULL,
    source text NOT NULL,
    via text NOT NULL,
    kind text,
    at timestamp with time zone,
    key text,
    seen_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    PRIMARY KEY (resource_id, link_id),
    CONSTRAINT derived_link_link_id_check CHECK ((link_id ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT derived_link_uri_check CHECK ((octet_length(uri) BETWEEN 8 AND 2048)),
    CONSTRAINT derived_link_site_check CHECK ((octet_length(site) BETWEEN 1 AND 128)),
    CONSTRAINT derived_link_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$')),
    CONSTRAINT derived_link_via_check CHECK ((via ~ '^[a-z0-9_]{1,32}$')),
    CONSTRAINT derived_link_kind_check CHECK ((kind IS NULL OR kind ~ '^[a-z_]{1,32}$')),
    CONSTRAINT derived_link_key_check CHECK ((key IS NULL OR octet_length(key) BETWEEN 3 AND 512))
);
CREATE INDEX derived_link_named_idx ON derived_link USING btree (link_id) WHERE key IS NOT NULL;
CREATE INDEX derived_link_key_idx ON derived_link USING btree (key) WHERE key IS NOT NULL;

-- Resources whose sources are due: after a save that changed the document, and when the
-- shortest source lifetime has passed. Failures back off like grouping's.
CREATE TABLE also_on_dirty (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    last_error text
);
CREATE INDEX also_on_dirty_due_idx ON also_on_dirty USING btree (next_attempt_at, resource_id);

-- A saved document (not a refetch that changed nothing) makes its sources due.
CREATE FUNCTION mark_also_on_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings') AND NEW.status IS NOT DISTINCT FROM OLD.status THEN RETURN NULL; END IF;
 INSERT INTO also_on_dirty(resource_id) VALUES(NEW.id)
  ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=least(also_on_dirty.next_attempt_at,now());
 RETURN NULL;
END $$;
CREATE TRIGGER also_on_resource_dirty AFTER INSERT OR UPDATE OF doc, status ON resource FOR EACH ROW EXECUTE FUNCTION mark_also_on_dirty();

-- A changed key regroups its resource; changed keys, facts and links change what a search finds
-- and shows.
CREATE TRIGGER resource_key_group_dirty AFTER INSERT OR DELETE OR UPDATE OF key ON resource_key FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER resource_key_group_revision AFTER INSERT OR DELETE OR UPDATE OF key ON resource_key FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
CREATE TRIGGER resource_key_revision BEFORE INSERT OR DELETE OR UPDATE ON resource_key FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER resource_fact_revision BEFORE INSERT OR DELETE OR UPDATE ON resource_fact FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER derived_link_revision BEFORE INSERT OR DELETE OR UPDATE ON derived_link FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

-- A keyed derived link is evidence for the tagged resource it names: a change regroups it.
CREATE FUNCTION mark_named_group_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ DECLARE named text[];
BEGIN
 IF TG_OP='UPDATE' AND OLD.key IS NOT DISTINCT FROM NEW.key AND OLD.link_id=NEW.link_id THEN RETURN NULL; END IF;
 named := ARRAY[CASE WHEN TG_OP<>'INSERT' AND OLD.key IS NOT NULL THEN OLD.link_id END,
                CASE WHEN TG_OP<>'DELETE' AND NEW.key IS NOT NULL THEN NEW.link_id END];
 INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=ANY(named) ON CONFLICT DO NOTHING;
 UPDATE resource SET group_revision=group_revision+1 WHERE id=ANY(named);
 RETURN NULL;
END $$;
CREATE TRIGGER derived_link_named_group_dirty AFTER INSERT OR DELETE OR UPDATE ON derived_link FOR EACH ROW EXECUTE FUNCTION mark_named_group_dirty();

-- A card's `also_on`: the other members of its group (tagged links, with their ID: other
-- resources first, one each by ID, then other units of its own resource), then the live derived
-- links of the card's resource and of every member, by service name, each URL once and never
-- the card itself; at most `cap` entries. `resource` is the card's resource ID for a resource's
-- card, and the unit for an occurrence or an event reference (whose feed's links are not its
-- own). `members` false (groups switched off) leaves the group out. Read at
-- request time: OSM-derived values never enter these tables.
-- A card (`per_site`) shows one entry per service. A tagged member whose URL is on a service
-- (`doc.service`: a music service, or one a derived link names, such as Wikidata's F-Droid link)
-- takes that service's place ahead of the derived links (on music, the member on the recording's
-- original album first, then by ID); otherwise the derived link most members name. A page that
-- only calls itself after a service (its own `site`) takes no service's place, and other tagged
-- members show one entry each. The card's own service, by its URL, has its link, the card, so it
-- shows no other page there. The details read (`per_site` false) lists them all.
CREATE FUNCTION also_on(unit text, resource text, cap integer, members boolean, per_site boolean DEFAULT true) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 WITH grp AS (SELECT group_id,resource_id FROM group_unit WHERE members AND unit_id=unit AND group_id IS NOT NULL),
 -- Before any document is read: one unit for each of up to `cap` other resources (by ID), then
 -- up to `cap` more units of the card's own resource (a feed's other occurrences), and the
 -- unit itself. A resource with many units never crowds the others out.
 others AS (SELECT DISTINCT ON (u.resource_id) u.unit_id FROM grp JOIN group_unit u USING(group_id)
   WHERE u.resource_id<>grp.resource_id ORDER BY u.resource_id,u.unit_id LIMIT cap),
 siblings AS (SELECT u.unit_id FROM grp JOIN group_unit u USING(group_id,resource_id)
   WHERE u.unit_id<>unit ORDER BY u.unit_id LIMIT cap),
 picked AS (SELECT unit_id FROM others UNION SELECT unit_id FROM siblings UNION SELECT unit FROM grp),
 m AS (SELECT d.id,CASE WHEN d.occurrence IS NULL THEN d.uri ELSE coalesce(nullif(d.doc->>'url',''),d.uri) END AS uri,
   d.host,d.doc->>'site' AS site,d.doc->>'service' AS service,d.osm_type IS NOT NULL AS osm,d.unit_id=unit AS own,d.id=(SELECT resource_id FROM grp) AS mine,
   d.type IN ('MusicRecording','MusicAlbum','MusicGroup','MusicPlaylist') AS music,
   (SELECT f.value FROM resource_fact f WHERE f.resource_id=d.id AND f.name='album' AND f.expires_at>now() ORDER BY f.value LIMIT 1) AS album
   FROM picked p
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=p.unit_id OFFSET 0) d),
 -- The service a member's URL is on: the one its save recorded (`doc.service`) or an OSM object's
 -- OpenStreetMap. Never what a page says about itself (its `site`, its type).
 tagged AS (SELECT id,uri,min(host) AS host,min(site) AS site,
     coalesce(min(service),CASE WHEN bool_or(osm) THEN 'OpenStreetMap' END) AS service,
     bool_or(mine) AS mine,bool_or(music) AS music,min(album) AS album FROM m GROUP BY id,uri HAVING NOT bool_or(own)),
 owners AS (SELECT resource AS resource_id UNION SELECT u.resource_id FROM grp JOIN group_unit u USING(group_id)),
 original AS (SELECT lower(min(f.value)) AS title FROM resource_fact f JOIN owners o USING(resource_id) WHERE f.name='original_album' AND f.expires_at>now()),
 -- The services the live derived links name, before those already tagged members are dropped:
 -- two tagged spellings of one F-Droid page share its slot even when the derived link is one of them.
 services AS (SELECT DISTINCT lower(l.site) AS service FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND l.site IS NOT NULL),
 derived AS (SELECT DISTINCT ON (l.link_id) l.link_id,l.uri,l.site,l.via,l.kind,l.at,
     count(*) OVER (PARTITION BY l.link_id) AS named
   FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND NOT EXISTS(SELECT 1 FROM owners x WHERE x.resource_id=l.link_id)
     AND NOT EXISTS(SELECT 1 FROM tagged t WHERE t.id=l.link_id)
   ORDER BY l.link_id,l.seen_at DESC,l.via),
 entries AS (SELECT CASE WHEN mine THEN 1 ELSE 0 END AS o,id AS k1,uri AS k2,
     CASE WHEN starts_with(lower(album),(SELECT title FROM original)) THEN -1 ELSE 0 END::bigint AS rank,
     CASE WHEN service IS NOT NULL AND (music OR lower(service) IN (SELECT service FROM services)) THEN lower(service) ELSE 'member:'||id END AS service,
     jsonb_build_object('id',id,'uri',uri,'host',host,'site',site) AS e FROM tagged
   UNION ALL SELECT 2,lower(site),uri,-named,lower(site),jsonb_strip_nulls(jsonb_build_object('uri',uri,'site',site,'derived',true,'via',via,'kind',kind,'at',at)) FROM derived),
 -- The card's own service, by its URL alone, like a member's.
 own AS (SELECT lower(coalesce(r.doc->>'service',CASE WHEN r.osm_type IS NOT NULL THEN 'OpenStreetMap' END)) AS service FROM resource r WHERE r.id=also_on.resource),
 shown AS (SELECT * FROM (SELECT *,row_number() OVER (PARTITION BY service ORDER BY o,rank,k2) AS nth FROM entries) x
   WHERE NOT per_site OR (nth=1 AND service IS DISTINCT FROM (SELECT service FROM own)))
 SELECT coalesce((SELECT jsonb_agg(e ORDER BY o,k1,k2) FROM (SELECT * FROM shown ORDER BY o,k1,k2 LIMIT cap) x),'[]'::jsonb)
$$;


-- ── The same-event hint ──────────────────────────────────────────────────────────────────────

-- Event units (an event resource, or one occurrence of a feed) on different pages that are
-- possibly the same event. A hint only: never a group, never counted, never ranks. A background
-- pass (same_event::drain) writes the pairs; cards only read them.

-- A unit the pass has read, with what pairs are matched on: its start, normalised name
-- (`same_event_name`) and the numbers it keeps, place, venue (resource or OSM object), organizer
-- URL and zone; and the `fingerprint` of the values they came from, so a change is read again.
-- `refill:{fingerprint}` marks one whose list lost a pair to another's change: it is read again
-- to refill its list. Pairs are matched in this table, through its start and geography indexes.
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

-- One row per direction of a pair: `score` is the similarity of the two normalised names. A unit
-- names at most 20 and is named by at most 40.
CREATE TABLE same_event (
    unit_id text NOT NULL REFERENCES same_event_unit(unit_id) ON DELETE CASCADE,
    other_id text NOT NULL REFERENCES same_event_unit(unit_id) ON DELETE CASCADE,
    score real NOT NULL,
    PRIMARY KEY (unit_id, other_id),
    CONSTRAINT same_event_pair CHECK (unit_id <> other_id)
);
CREATE INDEX same_event_other_idx ON same_event USING btree (other_id);

-- Resources whose event units may have changed (start, place or name), entered the window, or
-- lost a pair.
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
  -- A removed occurrence locks its feed's row before marking it, as a save does (its row, then
  -- the mark) and as an inserted occurrence's foreign key does.
  PERFORM 1 FROM resource WHERE id=OLD.resource_id FOR KEY SHARE;
  IF NOT FOUND THEN RETURN NULL; END IF;
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


-- ── Rows the code updates in place ───────────────────────────────────────────────────────────

-- The search revision starts at 1.
INSERT INTO search_write DEFAULT VALUES;
INSERT INTO group_runtime DEFAULT VALUES;
