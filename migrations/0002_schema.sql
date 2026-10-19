-- Pinakes baseline schema: tables, views, functions and triggers (see 0001_extensions.sql).

-- Functions come first and may name tables created below.
SET LOCAL check_function_bodies = false;


CREATE FUNCTION bump_group_child_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 UPDATE resource SET group_revision=group_revision+1
 WHERE id IN (CASE WHEN TG_OP<>'INSERT' THEN OLD.resource_id END, CASE WHEN TG_OP<>'DELETE' THEN NEW.resource_id END);
 RETURN NULL;
END $$;

CREATE FUNCTION bump_group_resource_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 NEW.group_revision=OLD.group_revision+1;
 RETURN NEW;
END $$;

CREATE FUNCTION bump_search_revision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    UPDATE search_revision SET revision=revision+1 WHERE id=1;
    RETURN NULL;
END $$;

CREATE FUNCTION event_boundary(d jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT coalesce(d#>>'{venue,location_source}' NOT IN ('geocoded','osm'),true)
 OR (NOT d ? 'location' AND NOT (d->'venue') ? 'location')
$$;

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

-- The group hint on a unit's card: the group's ID and size and up to eight other members, each
-- as {id, uri, host, site} (the API derives the card's site from these): other resources first,
-- one entry each by ID, then other units of the card's own resource by URI. Read at request
-- time: OSM-derived values never enter the group tables.
CREATE FUNCTION group_hint(unit text) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 WITH grp AS (SELECT group_id,resource_id FROM group_unit WHERE unit_id=unit AND group_id IS NOT NULL),
 -- Before any document is read: one unit for each of up to eight other resources (by ID),
 -- then up to eight more units of the card's own resource (a feed's other occurrences), and
 -- the unit itself. A resource with many units never crowds the others out.
 others AS (SELECT DISTINCT ON (u.resource_id) u.unit_id FROM grp JOIN group_unit u USING(group_id)
   WHERE u.resource_id<>grp.resource_id ORDER BY u.resource_id,u.unit_id LIMIT 8),
 siblings AS (SELECT u.unit_id FROM grp JOIN group_unit u USING(group_id,resource_id)
   WHERE u.unit_id<>unit ORDER BY u.unit_id LIMIT 8),
 picked AS (SELECT unit_id FROM others UNION SELECT unit_id FROM siblings UNION SELECT unit),
 m AS (SELECT d.id,CASE WHEN d.occurrence IS NULL THEN d.uri ELSE coalesce(nullif(d.doc->>'url',''),d.uri) END AS uri,
   d.host,d.doc->>'site' AS site,d.unit_id=unit AS own,d.id=(SELECT resource_id FROM grp) AS mine
   FROM picked p
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=p.unit_id OFFSET 0) d)
 SELECT jsonb_build_object('id',grp.group_id,
  'size',(SELECT count(*) FROM group_unit x WHERE x.group_id=grp.group_id),
  'members',coalesce((SELECT jsonb_agg(to_jsonb(o)-'mine' ORDER BY o.mine,o.id,o.uri) FROM
    (SELECT id,uri,min(host) AS host,min(site) AS site,bool_or(mine) AS mine FROM m GROUP BY id,uri HAVING NOT bool_or(own) ORDER BY bool_or(mine),id,uri LIMIT 8) o),'[]'::jsonb))
 FROM grp
$$;

CREATE FUNCTION group_member_deleted() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 DELETE FROM group_input WHERE resource_id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id);
 INSERT INTO group_dirty(resource_id) SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id ON CONFLICT DO NOTHING;
 DELETE FROM "group" g WHERE id=OLD.group_id AND NOT curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id);
 RETURN NULL;
END $$;

CREATE FUNCTION mark_group_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings') THEN RETURN NULL; END IF;
  INSERT INTO group_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;

CREATE FUNCTION reset_denial_streak() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF NEW.last_error IS NULL OR NEW.last_error NOT IN('upstream_denied','graphql') THEN
  NEW.denial_streak=0; NEW.denied_since=NULL;
 END IF;
 RETURN NEW;
END $$;

SET default_tablespace = '';

SET default_table_access_method = heap;

CREATE TABLE osm_place (
    osm_type text NOT NULL,
    osm_id bigint NOT NULL,
    point geometry(Point,4326),
    bbox double precision[],
    name text,
    category text,
    address jsonb DEFAULT '{}'::jsonb NOT NULL,
    tags jsonb DEFAULT '{}'::jsonb NOT NULL,
    display_name text,
    type text NOT NULL,
    type_path text[] NOT NULL,
    doc jsonb NOT NULL,
    fetched_at timestamp with time zone NOT NULL,
    refresh_at timestamp with time zone NOT NULL,
    status text NOT NULL,
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, ((COALESCE(name, ''::text) || ' '::text) || COALESCE(display_name, ''::text)))) STORED,
    fetch_doc jsonb NOT NULL,
    fetch_display_name text,
    versions jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT osm_place_doc_check CHECK ((octet_length((doc)::text) <= 8192)),
    CONSTRAINT osm_place_osm_id_check CHECK ((osm_id > 0)),
    CONSTRAINT osm_place_osm_type_check CHECK ((osm_type = ANY (ARRAY['node'::text, 'way'::text, 'relation'::text]))),
    CONSTRAINT osm_place_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'missing'::text])))
);

COMMENT ON TABLE osm_place IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';

CREATE TABLE resource_change (
    seq bigint NOT NULL,
    resource_id text NOT NULL,
    occurrence_key text,
    at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    class text NOT NULL,
    fields text[] NOT NULL,
    before jsonb,
    after jsonb,
    source text NOT NULL,
    osm_type text,
    osm_id bigint,
    CONSTRAINT change_event_boundary CHECK ((event_boundary(before) AND event_boundary(after))),
    CONSTRAINT change_web_osm_boundary CHECK (((source = ANY (ARRAY['osm'::text, 'geo'::text])) OR ((COALESCE((before ->> 'url'::text), ''::text) !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'::text) AND (COALESCE((after ->> 'url'::text), ''::text) !~ '^https?://([a-z0-9-]+[.])*(openstreetmap[.]org|osm[.]org)[.]?(:[0-9]+)?([/?#]|$)'::text)))),
    CONSTRAINT resource_change_check CHECK (((source <> 'osm'::text) OR ((osm_type IS NOT NULL) AND (osm_id IS NOT NULL) AND ((before IS NULL) OR ((before - ARRAY['osm_id'::text, 'version'::text]) = '{}'::jsonb)) AND ((after IS NULL) OR ((after - ARRAY['osm_id'::text, 'version'::text]) = '{}'::jsonb))))),
    CONSTRAINT resource_change_class_check CHECK ((class = ANY (ARRAY['created'::text, 'rescheduled'::text, 'relocated'::text, 'cancelled'::text, 'uncancelled'::text, 'renamed'::text, 'details'::text, 'deleted'::text, 'aliased'::text])))
);

CREATE VIEW change_document AS
 SELECT c.seq,
    c.resource_id,
    c.occurrence_key,
    c.at,
    c.class,
    c.fields,
    c.source,
        CASE
            WHEN ((c.source = 'osm'::text) AND (c.before IS NOT NULL)) THEN (o.versions -> (c.before ->> 'version'::text))
            ELSE event_document(c.before)
        END AS before,
        CASE
            WHEN ((c.source = 'osm'::text) AND (c.after IS NOT NULL)) THEN (o.versions -> (c.after ->> 'version'::text))
            ELSE event_document(c.after)
        END AS after,
        CASE
            WHEN ((c.source = 'osm'::text) OR ((c.before #>> '{venue,osm_id}'::text[]) IS NOT NULL) OR ((c.after #>> '{venue,osm_id}'::text[]) IS NOT NULL)) THEN '© OpenStreetMap contributors (ODbL)'::text
            ELSE NULL::text
        END AS attribution
   FROM (resource_change c
     LEFT JOIN osm_place o ON (((o.osm_type = c.osm_type) AND (o.osm_id = c.osm_id))));

-- The same object under another URI (alias::root, alias::canonical): never fetched, its
-- references count for the root and reads answer with the root.
CREATE TABLE resource_alias (
    alias_id text NOT NULL,
    root_id text NOT NULL,
    basis text NOT NULL,
    CONSTRAINT resource_alias_check CHECK ((alias_id <> root_id)),
    CONSTRAINT resource_alias_basis CHECK ((basis = ANY (ARRAY['cursor'::text, 'osm'::text, 'identifier'::text, 'tracking'::text, 'canonical'::text])))
);

CREATE TABLE reference (
    resource_id text NOT NULL,
    kind text NOT NULL,
    author text NOT NULL,
    label text,
    object_path text NOT NULL,
    recurrence_id text,
    start_at timestamp with time zone,
    end_at timestamp with time zone,
    created_at timestamp with time zone NOT NULL,
    unit_key text DEFAULT ''::text NOT NULL,
    event_status text,
    CONSTRAINT reference_event_status CHECK (((event_status IS NULL) OR (event_status = ANY (ARRAY['scheduled'::text, 'cancelled'::text, 'postponed'::text, 'rescheduled'::text, 'moved_online'::text])))),
    CONSTRAINT reference_kind_check CHECK ((kind = ANY (ARRAY['tag'::text, 'reply'::text, 'embed'::text, 'collection'::text, 'event'::text])))
);

CREATE VIEW effective_reference AS
 SELECT COALESCE(a.root_id, f.resource_id) AS resource_id,
    f.author,
    f.object_path,
    f.kind,
    f.label,
    f.created_at
   FROM (reference f
     LEFT JOIN resource_alias a ON ((a.alias_id = f.resource_id)));

CREATE TABLE event_fetch (
    resource_id text NOT NULL,
    validators jsonb DEFAULT '{}'::jsonb NOT NULL,
    last_snapshot_at bigint,
    held_keys text[],
    missing_keys jsonb DEFAULT '{}'::jsonb NOT NULL,
    hold_count integer DEFAULT 0 NOT NULL,
    retention_days integer DEFAULT 30 NOT NULL,
    horizon_start timestamp with time zone,
    horizon_end timestamp with time zone,
    adapter_cursor text,
    adapter_resync boolean DEFAULT false NOT NULL,
    CONSTRAINT event_fetch_adapter_cursor_check CHECK ((octet_length(adapter_cursor) <= 4096)),
    CONSTRAINT event_fetch_retention_days_check CHECK (((retention_days >= 1) AND (retention_days <= 3650)))
);

CREATE TABLE occurrence (
    resource_id text NOT NULL,
    uid text NOT NULL,
    recurrence_id text DEFAULT ''::text NOT NULL,
    start_at timestamp with time zone NOT NULL,
    end_at timestamp with time zone,
    name text,
    venue_id text,
    status text NOT NULL,
    url text,
    doc jsonb DEFAULT '{}'::jsonb NOT NULL,
    point geometry(Point,4326),
    sequence bigint DEFAULT 0 NOT NULL,
    modified timestamp with time zone,
    content_hash text DEFAULT ''::text NOT NULL,
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, ((COALESCE(name, ''::text) || ' '::text) || COALESCE((doc ->> 'description'::text), ''::text)))) STORED,
    CONSTRAINT occurrence_document_cap CHECK ((octet_length((doc)::text) <= 8192)),
    CONSTRAINT occurrence_event_boundary CHECK ((event_boundary(doc) AND (COALESCE(((doc #>> '{venue,location_source}'::text[]) <> ALL (ARRAY['geocoded'::text, 'osm'::text])), true) OR (point IS NULL)))),
    CONSTRAINT occurrence_status CHECK ((status = ANY (ARRAY['scheduled'::text, 'cancelled'::text, 'postponed'::text, 'rescheduled'::text, 'moved_online'::text, 'superseded'::text])))
);

CREATE VIEW event_occurrence AS
 SELECT o.resource_id,
    o.uid,
    o.recurrence_id,
    o.start_at,
    o.end_at,
    o.name,
    o.venue_id,
    o.status,
    o.url,
    o.doc,
    o.point,
    o.sequence,
    o.modified,
    o.content_hash,
    o.search_text,
    o.doc AS document,
    o.point AS effective_point
   FROM occurrence o
  WHERE (((o.doc #>> '{venue,osm_id}'::text[]) IS NULL) AND (o.status <> 'superseded'::text))
UNION ALL
 SELECT x.resource_id,
    x.uid,
    x.recurrence_id,
    x.start_at,
    x.end_at,
    x.name,
    x.venue_id,
    x.status,
    x.url,
    x.doc,
    x.point,
    x.sequence,
    x.modified,
    x.content_hash,
    x.search_text,
    event_document(x.doc) AS document,
    o.point AS effective_point
   FROM (occurrence x
     LEFT JOIN osm_place o ON (((o.osm_type = split_part((x.doc #>> '{venue,osm_id}'::text[]), '/'::text, 1)) AND (o.osm_id =
        CASE
            WHEN (split_part((x.doc #>> '{venue,osm_id}'::text[]), '/'::text, 2) ~ '^[0-9]{1,18}$'::text) THEN (split_part((x.doc #>> '{venue,osm_id}'::text[]), '/'::text, 2))::bigint
            ELSE NULL::bigint
        END))))
  WHERE (((x.doc #>> '{venue,osm_id}'::text[]) IS NOT NULL) AND (x.status <> 'superseded'::text));

CREATE TABLE feed_cursor (
    nexus text NOT NULL,
    cursor bigint DEFAULT 0 NOT NULL,
    epoch text,
    feed_length bigint DEFAULT 0 NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    resets bigint DEFAULT 0 NOT NULL,
    CONSTRAINT feed_cursor_cursor_check CHECK ((cursor >= 0)),
    CONSTRAINT feed_cursor_feed_length_check CHECK ((feed_length >= 0))
);

CREATE TABLE "group" (
    id text NOT NULL,
    curated boolean DEFAULT false NOT NULL,
    CONSTRAINT group_id_check CHECK ((id ~ '^[0-9a-f]{32}$'::text))
);

CREATE TABLE group_dirty (
    resource_id text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    last_error text,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    fence_attempts integer DEFAULT 0 NOT NULL,
    resume_key text,
    stopped_fingerprint text,
    stopped_runs integer DEFAULT 0 NOT NULL
);

CREATE TABLE group_input (
    resource_id text NOT NULL,
    fingerprint text NOT NULL,
    parked_keys integer DEFAULT 0 NOT NULL,
    settled_stopped boolean DEFAULT false NOT NULL
);

CREATE TABLE group_unit (
    unit_id text NOT NULL,
    resource_id text NOT NULL,
    occurrence_key text DEFAULT ''::text NOT NULL,
    group_id text,
    occurrence jsonb,
    event jsonb,
    admitted_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT group_unit_unit_id_check CHECK ((unit_id ~ '^[0-9a-f]{32}$'::text))
);

CREATE TABLE resource (
    id text NOT NULL,
    uri text NOT NULL,
    host text,
    type text,
    type_path text[] DEFAULT '{}'::text[],
    name text,
    category text,
    point geometry(Point,4326),
    start_at timestamp with time zone,
    end_at timestamp with time zone,
    venue_id text,
    doc jsonb DEFAULT '{}'::jsonb NOT NULL,
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, ((COALESCE(name, ''::text) || ' '::text) || COALESCE((doc ->> 'description'::text), ''::text)))) STORED,
    resolver text,
    resolved_at timestamp with time zone,
    refresh_at timestamp with time zone,
    status text DEFAULT 'pending'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    orphaned_at timestamp with time zone DEFAULT now(),
    attempts integer DEFAULT 0 NOT NULL,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    lease_until timestamp with time zone,
    generation bigint DEFAULT 0 NOT NULL,
    dead_letter boolean DEFAULT false NOT NULL,
    last_error text,
    osm_type text,
    osm_id bigint,
    final boolean DEFAULT false NOT NULL,
    missing_streak integer DEFAULT 0 NOT NULL,
    last_missing_at timestamp with time zone,
    priority_at timestamp with time zone,
    osm_version text,
    ever_existed boolean DEFAULT false NOT NULL,
    timeout_streak integer DEFAULT 0 NOT NULL,
    denial_streak integer DEFAULT 0 NOT NULL,
    denied_since timestamp with time zone,
    group_revision bigint DEFAULT 0 NOT NULL,
    CONSTRAINT resource_denial_streak_check CHECK ((denial_streak >= 0)),
    CONSTRAINT resource_doc_check CHECK ((octet_length((doc)::text) <= 8192)),
    CONSTRAINT resource_event_boundary CHECK ((event_boundary(doc) AND (COALESCE(((doc #>> '{venue,location_source}'::text[]) <> ALL (ARRAY['geocoded'::text, 'osm'::text])), true) OR (point IS NULL)))),
    CONSTRAINT resource_event_status CHECK ((((doc ->> 'event_status'::text) IS NULL) OR ((doc ->> 'event_status'::text) = ANY (ARRAY['scheduled'::text, 'cancelled'::text, 'postponed'::text, 'rescheduled'::text, 'moved_online'::text])))),
    CONSTRAINT resource_id_check CHECK ((id ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT resource_osm_separation CHECK ((((osm_id IS NULL) AND (osm_type IS NULL)) OR ((osm_id IS NOT NULL) AND (osm_type IS NOT NULL) AND (osm_id > 0) AND (osm_type = ANY (ARRAY['node'::text, 'way'::text, 'relation'::text])) AND (NOT (resolver IS DISTINCT FROM 'osm'::text)) AND (name IS NULL) AND (point IS NULL) AND (category IS NULL) AND (type IS NULL) AND (type_path IS NULL) AND (NOT ((doc ->> 'type'::text) IS DISTINCT FROM 'Place'::text)) AND ((doc - ARRAY['type'::text, 'url'::text, 'osm_id'::text, 'source'::text, 'status'::text]) = '{}'::jsonb)))),
    CONSTRAINT resource_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'pending'::text, 'missing'::text, 'unreachable'::text, 'error'::text]))),
    CONSTRAINT resource_timeout_streak_check CHECK ((timeout_streak >= 0)),
    CONSTRAINT resource_web_osm_boundary CHECK (((resolver IS DISTINCT FROM 'web'::text) OR (lower(rtrim(host, '.'::text)) !~ '(^|[.])(openstreetmap[.]org|osm[.]org)$'::text) OR ((name IS NULL) AND (point IS NULL) AND (category IS NULL) AND (type IS NULL) AND (doc = '{}'::jsonb))))
);

CREATE VIEW resource_document AS
 SELECT r.id,
    r.uri,
    r.host,
    r.type,
    r.type_path,
    r.name,
    r.category,
    r.point,
    r.start_at,
    r.end_at,
    r.venue_id,
    r.doc,
    r.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id
   FROM resource r
  WHERE ((r.osm_id IS NULL) AND ((r.doc #>> '{venue,osm_id}'::text[]) IS NULL))
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    r.type,
    r.type_path,
    r.name,
    r.category,
    o.point,
    r.start_at,
    r.end_at,
    r.venue_id,
    event_document(r.doc) AS doc,
    r.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id
   FROM (resource r
     LEFT JOIN osm_place o ON (((o.osm_type = split_part((r.doc #>> '{venue,osm_id}'::text[]), '/'::text, 1)) AND (o.osm_id =
        CASE
            WHEN (split_part((r.doc #>> '{venue,osm_id}'::text[]), '/'::text, 2) ~ '^[0-9]{1,18}$'::text) THEN (split_part((r.doc #>> '{venue,osm_id}'::text[]), '/'::text, 2))::bigint
            ELSE NULL::bigint
        END))))
  WHERE ((r.osm_id IS NULL) AND ((r.doc #>> '{venue,osm_id}'::text[]) IS NOT NULL))
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    o.type,
    o.type_path,
    o.name,
    o.category,
    o.point,
    r.start_at,
    r.end_at,
    r.venue_id,
    COALESCE(((o.versions -> r.osm_version) || jsonb_build_object('url', r.uri, 'fetched_at', ((EXTRACT(epoch FROM r.resolved_at) * (1000)::numeric))::bigint, 'ttl_secs', GREATEST((0)::bigint, (EXTRACT(epoch FROM (r.refresh_at - r.resolved_at)))::bigint))), '{}'::jsonb) AS doc,
    o.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id
   FROM (resource r
     LEFT JOIN osm_place o ON (((o.osm_type = r.osm_type) AND (o.osm_id = r.osm_id) AND (r.osm_version IS NOT NULL))))
  WHERE (r.osm_id IS NOT NULL);

CREATE VIEW group_member_document AS
 SELECT r.id,
    r.uri,
    r.host,
    r.type,
    r.type_path,
    r.name,
    r.category,
    r.point,
    r.start_at,
    r.end_at,
    r.venue_id,
    r.doc,
    r.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id,
    u.unit_id,
    u.occurrence,
    u.event,
    NULL::text AS event_author
   FROM (group_unit u
     CROSS JOIN LATERAL ( SELECT resource_document.id,
            resource_document.uri,
            resource_document.host,
            resource_document.type,
            resource_document.type_path,
            resource_document.name,
            resource_document.category,
            resource_document.point,
            resource_document.start_at,
            resource_document.end_at,
            resource_document.venue_id,
            resource_document.doc,
            resource_document.search_text,
            resource_document.resolver,
            resource_document.resolved_at,
            resource_document.refresh_at,
            resource_document.status,
            resource_document.osm_type,
            resource_document.osm_id
           FROM resource_document
          WHERE (resource_document.id = u.resource_id)
         OFFSET 0) r)
  WHERE ((u.occurrence IS NULL) AND (u.event IS NULL))
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    (o.doc ->> 'type'::text) AS type,
    ARRAY( SELECT jsonb_array_elements_text((o.doc -> 'type_path'::text)) AS jsonb_array_elements_text) AS type_path,
    o.name,
    NULL::text AS category,
    o.effective_point AS point,
    o.start_at,
    o.end_at,
    o.venue_id,
    o.document AS doc,
    o.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    NULL::text AS osm_type,
    NULL::bigint AS osm_id,
    u.unit_id,
    u.occurrence,
    NULL::jsonb AS event,
    NULL::text AS event_author
   FROM ((group_unit u
     CROSS JOIN LATERAL ( SELECT event_occurrence.resource_id,
            event_occurrence.uid,
            event_occurrence.recurrence_id,
            event_occurrence.start_at,
            event_occurrence.end_at,
            event_occurrence.name,
            event_occurrence.venue_id,
            event_occurrence.status,
            event_occurrence.url,
            event_occurrence.doc,
            event_occurrence.point,
            event_occurrence.sequence,
            event_occurrence.modified,
            event_occurrence.content_hash,
            event_occurrence.search_text,
            event_occurrence.document,
            event_occurrence.effective_point
           FROM event_occurrence
          WHERE ((event_occurrence.resource_id = u.resource_id) AND (event_occurrence.uid = (u.occurrence ->> 'uid'::text)) AND (event_occurrence.recurrence_id = COALESCE((u.occurrence ->> 'recurrence_id'::text), ''::text)))
         OFFSET 0) o)
     JOIN resource r ON ((r.id = u.resource_id)))
  WHERE ((u.occurrence IS NOT NULL) AND (r.status <> 'missing'::text))
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    'Event'::text AS type,
    ARRAY['Thing'::text, 'Event'::text] AS type_path,
    NULL::text AS name,
    NULL::text AS category,
    r.point,
    f.start_at,
    f.end_at,
    r.id AS venue_id,
    (jsonb_build_object('type', 'Event', 'type_path', jsonb_build_array('Thing', 'Event'), 'url', r.uri, 'site', COALESCE(r.host, ''::text), 'source', 'nexus_reference', 'status', 'ok', 'attributes', '{}'::jsonb, 'start', f.start_at, 'end', f.end_at, 'event_status', COALESCE(f.event_status, 'scheduled'::text)) ||
        CASE
            WHEN ((r.doc -> 'location'::text) IS NULL) THEN '{}'::jsonb
            ELSE jsonb_build_object('location', (r.doc -> 'location'::text))
        END) AS doc,
    ''::tsvector AS search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id,
    u.unit_id,
    NULL::jsonb AS occurrence,
    u.event,
    f.author AS event_author
   FROM ((group_unit u
     JOIN reference f ON (((f.resource_id = u.resource_id) AND (f.author = (u.event ->> 'author'::text)) AND (f.object_path = (u.event ->> 'object_path'::text)) AND (f.recurrence_id = COALESCE((u.event ->> 'recurrence_id'::text), ''::text)))))
     CROSS JOIN LATERAL ( SELECT resource_document.id,
            resource_document.uri,
            resource_document.host,
            resource_document.type,
            resource_document.type_path,
            resource_document.name,
            resource_document.category,
            resource_document.point,
            resource_document.start_at,
            resource_document.end_at,
            resource_document.venue_id,
            resource_document.doc,
            resource_document.search_text,
            resource_document.resolver,
            resource_document.resolved_at,
            resource_document.refresh_at,
            resource_document.status,
            resource_document.osm_type,
            resource_document.osm_id
           FROM resource_document
          WHERE (resource_document.id = u.resource_id)
         OFFSET 0) r)
  WHERE ((u.event IS NOT NULL) AND (f.kind = 'event'::text) AND (f.start_at IS NOT NULL));

CREATE TABLE group_override (
    unit_id text NOT NULL,
    group_id text NOT NULL
);

CREATE TABLE group_runtime (
    id boolean DEFAULT true NOT NULL,
    settings jsonb,
    CONSTRAINT group_runtime_id_check CHECK (id)
);

CREATE VIEW search_unit AS
 SELECT r.id,
    r.uri,
    r.host,
    r.type,
    r.type_path,
    r.name,
    r.category,
    r.point,
    r.start_at,
    r.end_at,
    r.venue_id,
    r.doc,
    r.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id,
    r.id AS unit_id,
    NULL::jsonb AS occurrence,
    NULL::jsonb AS event,
    NULL::text AS event_author
   FROM resource_document r
  WHERE ((NOT COALESCE((r.type_path @> ARRAY['Event'::text]), false)) OR (r.start_at IS NOT NULL)) AND (NOT (EXISTS ( SELECT 1
           FROM resource_alias a
          WHERE (a.alias_id = r.id))))
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    (o.doc ->> 'type'::text) AS type,
    ARRAY( SELECT jsonb_array_elements_text((o.doc -> 'type_path'::text)) AS jsonb_array_elements_text) AS type_path,
    o.name,
    NULL::text AS category,
    o.effective_point AS point,
    o.start_at,
    o.end_at,
    o.venue_id,
    o.document AS doc,
    o.search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    NULL::text AS osm_type,
    NULL::bigint AS osm_id,
    md5((jsonb_build_array(r.id, o.uid, o.recurrence_id))::text) AS unit_id,
    jsonb_build_object('uid', o.uid, 'recurrence_id', NULLIF(o.recurrence_id, ''::text), 'status', o.status) AS occurrence,
    NULL::jsonb AS event,
    NULL::text AS event_author
   FROM (event_occurrence o
     JOIN resource r ON ((r.id = o.resource_id)))
  WHERE (r.status <> 'missing'::text)
UNION ALL
 SELECT r.id,
    r.uri,
    r.host,
    'Event'::text AS type,
    ARRAY['Thing'::text, 'Event'::text] AS type_path,
    NULL::text AS name,
    NULL::text AS category,
    r.point,
    f.start_at,
    f.end_at,
    r.id AS venue_id,
    (jsonb_build_object('type', 'Event', 'type_path', jsonb_build_array('Thing', 'Event'), 'url', r.uri, 'site', COALESCE(r.host, ''::text), 'source', 'nexus_reference', 'status', 'ok', 'attributes', '{}'::jsonb, 'start', f.start_at, 'end', f.end_at, 'event_status', COALESCE(f.event_status, 'scheduled'::text)) ||
        CASE
            WHEN ((r.doc -> 'location'::text) IS NULL) THEN '{}'::jsonb
            ELSE jsonb_build_object('location', (r.doc -> 'location'::text))
        END) AS doc,
    ''::tsvector AS search_text,
    r.resolver,
    r.resolved_at,
    r.refresh_at,
    r.status,
    r.osm_type,
    r.osm_id,
    md5((jsonb_build_array(r.id, f.author, f.object_path, f.recurrence_id))::text) AS unit_id,
    NULL::jsonb AS occurrence,
    jsonb_build_object('author', f.author, 'object_path', f.object_path, 'recurrence_id', f.recurrence_id, 'start', f.start_at, 'end', f.end_at, 'status', COALESCE(f.event_status, 'scheduled'::text)) AS event,
    f.author AS event_author
   FROM ((reference f
     LEFT JOIN resource_alias a ON ((a.alias_id = f.resource_id)))
     JOIN resource_document r ON ((r.id = COALESCE(a.root_id, f.resource_id))))
  WHERE ((f.kind = 'event'::text) AND (f.start_at IS NOT NULL));

CREATE SEQUENCE resource_change_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE resource_change_seq_seq OWNED BY resource_change.seq;

CREATE TABLE resource_derivation (
    parent_id text NOT NULL,
    child_id text NOT NULL,
    adapter text NOT NULL,
    expires_at timestamp with time zone,
    until timestamp with time zone,
    first_seen_at timestamp with time zone DEFAULT now() NOT NULL,
    -- Feed row key -> the instant that row asks for (store::link_sources).
    sources jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT resource_derivation_check CHECK ((parent_id <> child_id))
);

CREATE TABLE resource_group (
    unit_id text NOT NULL,
    resource_id text NOT NULL,
    group_id text,
    key text NOT NULL,
    admitted_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT resource_group_key_check CHECK ((octet_length(key) <= 2200))
);

CREATE TABLE resource_score (
    resource_id text NOT NULL,
    score double precision NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    newest_at timestamp with time zone NOT NULL
);

CREATE TABLE search_revision (
    id integer NOT NULL,
    revision bigint NOT NULL,
    CONSTRAINT search_revision_id_check CHECK ((id = 1))
);

CREATE TABLE source_state (
    source text NOT NULL,
    last_success_at timestamp with time zone,
    last_error text,
    consecutive_failures integer DEFAULT 0 NOT NULL,
    change_totals jsonb DEFAULT '{}'::jsonb NOT NULL,
    feed_held_total bigint DEFAULT 0 NOT NULL,
    cursor_resets_total bigint DEFAULT 0 NOT NULL,
    canary_at timestamp with time zone,
    canary_ok boolean,
    canary_missing text[] DEFAULT '{}'::text[] NOT NULL,
    canary_failures integer DEFAULT 0 NOT NULL,
    canary_due_at timestamp with time zone,
    canary_lease_until timestamp with time zone,
    canary_generation bigint DEFAULT 0 NOT NULL,
    CONSTRAINT source_error_class CHECK (((last_error IS NULL) OR (last_error = ANY (ARRAY['dns'::text, 'connect'::text, 'timeout'::text, 'http_4xx'::text, 'http_5xx'::text, 'parse'::text, 'policy'::text]))))
);

ALTER TABLE ONLY resource_change ALTER COLUMN seq SET DEFAULT nextval('resource_change_seq_seq'::regclass);

ALTER TABLE ONLY resource_alias
    ADD CONSTRAINT resource_alias_pkey PRIMARY KEY (alias_id);

ALTER TABLE ONLY event_fetch
    ADD CONSTRAINT event_fetch_pkey PRIMARY KEY (resource_id);

ALTER TABLE ONLY feed_cursor
    ADD CONSTRAINT feed_cursor_pkey PRIMARY KEY (nexus);

ALTER TABLE ONLY group_dirty
    ADD CONSTRAINT group_dirty_pkey PRIMARY KEY (resource_id);

ALTER TABLE ONLY group_input
    ADD CONSTRAINT group_input_pkey PRIMARY KEY (resource_id);

ALTER TABLE ONLY group_override
    ADD CONSTRAINT group_override_pkey PRIMARY KEY (unit_id);

ALTER TABLE ONLY "group"
    ADD CONSTRAINT group_pkey PRIMARY KEY (id);

ALTER TABLE ONLY group_runtime
    ADD CONSTRAINT group_runtime_pkey PRIMARY KEY (id);


ALTER TABLE ONLY group_unit
    ADD CONSTRAINT group_unit_pkey PRIMARY KEY (unit_id);

ALTER TABLE ONLY occurrence
    ADD CONSTRAINT occurrence_pkey PRIMARY KEY (resource_id, uid, recurrence_id);

ALTER TABLE ONLY osm_place
    ADD CONSTRAINT osm_place_pkey PRIMARY KEY (osm_type, osm_id);

ALTER TABLE ONLY reference
    ADD CONSTRAINT reference_pkey PRIMARY KEY (author, object_path, unit_key);

ALTER TABLE ONLY resource_change
    ADD CONSTRAINT resource_change_pkey PRIMARY KEY (seq);

ALTER TABLE ONLY resource_derivation
    ADD CONSTRAINT resource_derivation_pkey PRIMARY KEY (parent_id, child_id);

ALTER TABLE ONLY resource_group
    ADD CONSTRAINT resource_group_pkey PRIMARY KEY (unit_id, key);

ALTER TABLE ONLY resource
    ADD CONSTRAINT resource_pkey PRIMARY KEY (id);

ALTER TABLE ONLY resource_score
    ADD CONSTRAINT resource_score_pkey PRIMARY KEY (resource_id);

ALTER TABLE ONLY resource
    ADD CONSTRAINT resource_uri_key UNIQUE (uri);

ALTER TABLE ONLY search_revision
    ADD CONSTRAINT search_revision_pkey PRIMARY KEY (id);

ALTER TABLE ONLY source_state
    ADD CONSTRAINT source_state_pkey PRIMARY KEY (source);

CREATE INDEX resource_alias_root ON resource_alias USING btree (root_id);

CREATE INDEX group_dirty_due_idx ON group_dirty USING btree (next_attempt_at, resource_id);

CREATE INDEX group_override_group_idx ON group_override USING btree (group_id);

CREATE INDEX group_unit_group_idx ON group_unit USING btree (group_id, resource_id, unit_id);

CREATE INDEX group_unit_resource_idx ON group_unit USING btree (resource_id);

CREATE INDEX occurrence_geography_idx ON occurrence USING gist (((point)::geography));

CREATE INDEX occurrence_hash_idx ON occurrence USING btree (resource_id, content_hash, start_at, name);

CREATE INDEX occurrence_page_idx ON occurrence USING btree (resource_id, start_at, uid, recurrence_id);

CREATE INDEX occurrence_point_idx ON occurrence USING gist (point);

CREATE INDEX occurrence_retention_idx ON occurrence USING btree (end_at, start_at);

CREATE INDEX occurrence_search_idx ON occurrence USING gin (search_text);

CREATE INDEX occurrence_start_idx ON occurrence USING btree (start_at);

CREATE INDEX occurrence_venue_idx ON occurrence USING btree (venue_id);

CREATE INDEX occurrence_venue_version_idx ON occurrence USING btree (((doc #>> '{venue,osm_id}'::text[])), ((doc #>> '{venue,osm_version}'::text[])));

CREATE INDEX osm_place_category_idx ON osm_place USING btree (category);

CREATE INDEX osm_place_geography_idx ON osm_place USING gist (((point)::geography));

CREATE INDEX osm_place_name_idx ON osm_place USING gin (name gin_trgm_ops);

CREATE INDEX osm_place_point_idx ON osm_place USING gist (point);

CREATE INDEX osm_place_search_idx ON osm_place USING gin (search_text);

CREATE INDEX osm_place_type_path_idx ON osm_place USING gin (type_path);

CREATE INDEX reference_author_idx ON reference USING btree (author);

CREATE INDEX reference_event_idx ON reference USING btree (resource_id, start_at) WHERE (kind = 'event'::text);

CREATE INDEX reference_label_idx ON reference USING btree (label, resource_id);

CREATE INDEX reference_resource_idx ON reference USING btree (resource_id);

CREATE INDEX reference_start_idx ON reference USING btree (start_at);

CREATE INDEX resource_category_idx ON resource USING btree (category);

CREATE INDEX resource_change_after_venue_idx ON resource_change USING btree (((after #>> '{venue,osm_id}'::text[])), ((after #>> '{venue,osm_version}'::text[])));

CREATE INDEX resource_change_at_idx ON resource_change USING btree (at);

CREATE INDEX resource_change_before_venue_idx ON resource_change USING btree (((before #>> '{venue,osm_id}'::text[])), ((before #>> '{venue,osm_version}'::text[])));

CREATE INDEX resource_change_osm_idx ON resource_change USING btree (osm_type, osm_id) WHERE (osm_type IS NOT NULL);

CREATE INDEX resource_change_resource_idx ON resource_change USING btree (resource_id, seq);

CREATE INDEX resource_claim_priority_idx ON resource USING btree (resolver, priority_at DESC NULLS LAST, next_attempt_at) WHERE ((NOT dead_letter) AND (NOT final));

CREATE INDEX resource_derivation_adapter_expiry ON resource_derivation USING btree (adapter, expires_at);

CREATE INDEX resource_derivation_child ON resource_derivation USING btree (child_id);

CREATE INDEX resource_geography_idx ON resource USING gist (((point)::geography));

CREATE INDEX resource_group_admission_idx ON resource_group USING btree (key, admitted_at, unit_id);

CREATE INDEX resource_group_group_idx ON resource_group USING btree (group_id);

CREATE INDEX resource_group_key_idx ON resource_group USING btree (key);

CREATE INDEX resource_group_resource_idx ON resource_group USING btree (resource_id);

CREATE INDEX resource_host_idx ON resource USING btree (host);

CREATE INDEX resource_name_trgm_idx ON resource USING gin (name gin_trgm_ops);

CREATE INDEX resource_orphaned_idx ON resource USING btree (orphaned_at) WHERE (orphaned_at IS NOT NULL);

CREATE INDEX resource_osm_idx ON resource USING btree (osm_type, osm_id) WHERE (osm_id IS NOT NULL);

CREATE INDEX resource_point_idx ON resource USING gist (point);

CREATE INDEX resource_queue_idx ON resource USING btree (resolver, next_attempt_at) WHERE (NOT dead_letter);

CREATE INDEX resource_search_idx ON resource USING gin (search_text);

CREATE INDEX resource_start_idx ON resource USING btree (start_at);

CREATE INDEX resource_type_path_idx ON resource USING gin (type_path);

CREATE INDEX resource_venue_idx ON resource USING btree (venue_id);

CREATE INDEX resource_venue_version_idx ON resource USING btree (((doc #>> '{venue,osm_id}'::text[])), ((doc #>> '{venue,osm_version}'::text[])));

CREATE TRIGGER group_member_deleted AFTER DELETE ON group_unit FOR EACH ROW EXECUTE FUNCTION group_member_deleted();

CREATE TRIGGER group_occurrence_dirty AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();

CREATE TRIGGER group_occurrence_revision AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();

CREATE TRIGGER group_reference_dirty AFTER INSERT OR DELETE OR UPDATE ON reference FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();

CREATE TRIGGER group_reference_revision AFTER INSERT OR DELETE OR UPDATE ON reference FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();

CREATE TRIGGER group_resource_dirty AFTER INSERT OR UPDATE OF doc ON resource FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();

-- A grouping run's fence (groups::fence) relies on this bump. alias::assign's UPDATE of the alias
-- names `doc` and `osm_version` so that it fires: keep those columns in both places.
CREATE TRIGGER group_resource_revision BEFORE UPDATE OF doc, status, uri, host, point, start_at, end_at, type, name, osm_version ON resource FOR EACH ROW EXECUTE FUNCTION bump_group_resource_revision();

CREATE TRIGGER group_units_revision BEFORE INSERT OR DELETE OR UPDATE ON group_unit FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER groups_revision BEFORE INSERT OR DELETE OR UPDATE ON "group" FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER occurrence_revision BEFORE INSERT OR DELETE OR UPDATE ON occurrence FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER osm_revision_insert BEFORE INSERT OR DELETE ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER osm_revision_update BEFORE UPDATE OF doc, status, name, type, type_path, category, point, bbox ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER reference_revision BEFORE INSERT OR DELETE OR UPDATE OR TRUNCATE ON reference FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER reset_denial_streak BEFORE UPDATE OF last_error ON resource FOR EACH ROW EXECUTE FUNCTION reset_denial_streak();

CREATE TRIGGER resource_revision_insert BEFORE INSERT OR DELETE ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER resource_revision_update BEFORE UPDATE OF uri, host, resolver, doc, status, name, type, type_path, category, point, start_at, end_at, osm_version, resolved_at, refresh_at ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE TRIGGER score_revision BEFORE INSERT OR DELETE OR UPDATE OR TRUNCATE ON resource_score FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

ALTER TABLE ONLY resource_alias
    ADD CONSTRAINT resource_alias_alias_id_fkey FOREIGN KEY (alias_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_alias
    ADD CONSTRAINT resource_alias_root_id_fkey FOREIGN KEY (root_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY event_fetch
    ADD CONSTRAINT event_fetch_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY group_dirty
    ADD CONSTRAINT group_dirty_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY group_input
    ADD CONSTRAINT group_input_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY group_override
    ADD CONSTRAINT group_override_group_id_fkey FOREIGN KEY (group_id) REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY group_override
    ADD CONSTRAINT group_override_unit_id_fkey FOREIGN KEY (unit_id) REFERENCES group_unit(unit_id) ON DELETE CASCADE;


ALTER TABLE ONLY group_unit
    ADD CONSTRAINT group_unit_group_id_fkey FOREIGN KEY (group_id) REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY group_unit
    ADD CONSTRAINT group_unit_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY occurrence
    ADD CONSTRAINT occurrence_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY reference
    ADD CONSTRAINT reference_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_derivation
    ADD CONSTRAINT resource_derivation_child_id_fkey FOREIGN KEY (child_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_derivation
    ADD CONSTRAINT resource_derivation_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_group
    ADD CONSTRAINT resource_group_group_id_fkey FOREIGN KEY (group_id) REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE ONLY resource_group
    ADD CONSTRAINT resource_group_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_group
    ADD CONSTRAINT resource_group_unit_id_fkey FOREIGN KEY (unit_id) REFERENCES group_unit(unit_id) ON DELETE CASCADE;

ALTER TABLE ONLY resource_score
    ADD CONSTRAINT resource_score_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resource(id) ON DELETE CASCADE;


-- Every instant taken from a source (event and reference times, a feed's horizon, a derived link's
-- expiry) lies in UTC years 0001–9999, the range Postgres reads back through its text form (the
-- code bounds them before saving; these constraints make it a guarantee). The other timestamp
-- columns are set from the server clock.
ALTER TABLE reference ADD CONSTRAINT reference_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00');
ALTER TABLE reference ADD CONSTRAINT reference_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00');
ALTER TABLE reference ADD CONSTRAINT reference_created_at_range CHECK (created_at >= '0001-01-01 00:00:00+00' AND created_at < '10000-01-01 00:00:00+00');
ALTER TABLE occurrence ADD CONSTRAINT occurrence_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00');
ALTER TABLE occurrence ADD CONSTRAINT occurrence_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00');
ALTER TABLE occurrence ADD CONSTRAINT occurrence_modified_range CHECK (modified >= '0001-01-01 00:00:00+00' AND modified < '10000-01-01 00:00:00+00');
ALTER TABLE resource ADD CONSTRAINT resource_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00');
ALTER TABLE resource ADD CONSTRAINT resource_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00');
ALTER TABLE event_fetch ADD CONSTRAINT event_fetch_horizon_start_range CHECK (horizon_start >= '0001-01-01 00:00:00+00' AND horizon_start < '10000-01-01 00:00:00+00');
ALTER TABLE event_fetch ADD CONSTRAINT event_fetch_horizon_end_range CHECK (horizon_end >= '0001-01-01 00:00:00+00' AND horizon_end < '10000-01-01 00:00:00+00');
ALTER TABLE resource_derivation ADD CONSTRAINT resource_derivation_expires_at_range CHECK (expires_at >= '0001-01-01 00:00:00+00' AND expires_at < '10000-01-01 00:00:00+00');
ALTER TABLE resource_derivation ADD CONSTRAINT resource_derivation_until_range CHECK (until >= '0001-01-01 00:00:00+00' AND until < '10000-01-01 00:00:00+00');

-- Singleton rows the code updates in place.
INSERT INTO search_revision(id,revision) VALUES(1,1);
INSERT INTO group_runtime DEFAULT VALUES;
