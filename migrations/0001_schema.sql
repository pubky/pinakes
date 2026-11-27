-- The Pinakes schema. Until the first deployment this is the only file: a schema change edits it,
-- and local and test databases are recreated (docs/operations.md, "Schema").
--
-- In order: extensions; resources and their references; events and OpenStreetMap places; the
-- read views; the search revision; groups; also-on; the same-event hint; catalogues; the rows the
-- code updates in place.

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

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
    -- The venue's resource (`doc.event.venue.resource_id`). A venue anything names is never
    -- deleted (the orphan cleanup skips it, and the key refuses it).
    venue_id text CONSTRAINT resource_venue_id_fkey REFERENCES resource(id),
    doc jsonb NOT NULL DEFAULT '{}' CONSTRAINT resource_doc_check CHECK (octet_length(doc::text) <= 8192),
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(doc->>'description', ''))) STORED,
    -- The route that reads it: one of `resolver::ROUTES` or `cards::KINDS` (a test holds the list
    -- equal to them); NULL for a row no route reads (an alias).
    resolver text CONSTRAINT resource_resolver_check CHECK (resolver IN ('web', 'pubky', 'geo', 'osm', 'event', 'event_platform', 'media', 'isbn', 'doi', 'nostr', 'activitypub', 'official')),
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
    -- The class of the last failure (`resolver::error_class`, a `RetryLater` class): a short name,
    -- never the error's text. Each resolver and source names its own, so the set is open.
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
    -- The catalogue card the card is built from (`catalogue_card.id`), while it is: such a row is
    -- never fetched (`keep_catalogue_backed`); it follows its catalogue card instead. No foreign
    -- key: an import removes cards in bulk while rows still name them, and the removal marks those
    -- rows (`catalogue_cards_deleted`), whose check (`catalogue::join::drain`) builds them again or
    -- gives them back to the fetch queue. A key would refuse the removal, or, cleared on delete,
    -- leave the rows final and out of the queue.
    catalogue_card bigint,
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
CREATE INDEX resource_catalogue_card_idx ON resource USING btree (catalogue_card) WHERE catalogue_card IS NOT NULL;
-- The labels of the cards built from catalogue cards, which a search's `tags=` matches.
CREATE INDEX resource_backed_labels_idx ON resource USING gin ((doc -> 'labels')) WHERE catalogue_card IS NOT NULL;
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

-- A resource whose last error is not a denial starts its denial streak over. The denials are
-- `queue::DENIAL_CLASSES` (a test holds the two lists equal).
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
CREATE INDEX resource_alias_root_idx ON resource_alias USING btree (root_id);

-- One reference from the Nexus feed (a tag, reply, embed, collection entry, or a post naming
-- itself), keyed by the object that makes it.
CREATE TABLE reference (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    kind text NOT NULL CONSTRAINT reference_kind_check CHECK (kind IN ('tag', 'reply', 'embed', 'collection', 'post')),
    author text NOT NULL,
    label text,
    object_path text NOT NULL,
    created_at timestamp with time zone NOT NULL,
    unit_key text NOT NULL DEFAULT '',
    PRIMARY KEY (author, object_path, unit_key),
    CONSTRAINT reference_created_at_range CHECK (created_at >= '0001-01-01 00:00:00+00' AND created_at < '10000-01-01 00:00:00+00')
);
-- Search's `related` filter reads each tagger's most recent references in its window; the
-- index serves every lookup by author too.
CREATE INDEX reference_author_created_idx ON reference USING btree (author, created_at);
-- `sort=trending` counts the references made inside a window.
CREATE INDEX reference_created_idx ON reference USING btree (created_at);
CREATE INDEX reference_label_idx ON reference USING btree (label, resource_id);
-- A resource's most recent references (`related` reads its tags in a window); serves every
-- lookup by resource too.
CREATE INDEX reference_resource_created_idx ON reference USING btree (resource_id, created_at);
-- `related`'s taggers are the authors of a resource's most recent tags: an index of its
-- tags alone, so a resource with many replies or embeds and few tags reads only tags.
CREATE INDEX reference_resource_tag_idx ON reference USING btree (resource_id, created_at) WHERE kind = 'tag';
-- A card's collections (`collections`): each link's collection posts newest first, in the order
-- the route lists them, so its count and every page read a bounded walk from the cursor on. Each
-- walk index serves its own probes and no others, whatever the planner's estimates:
-- - the link, author and file are in the "C" collation, as the probes compare them, so no index
--   of the default collation (the key by author and file, the resource's by time) can serve them;
-- - the instant is the expression `timezone('UTC', created_at)` (its UTC time, in the same order),
--   as the probes compare and order it, so no index of `created_at` itself (`trending`'s) can
--   serve them; `created_at` is included, for index-only scans;
-- - the predicates exclude each other's probes: only the link's walk states `object_path <> ''`,
--   only the viewer's `author <> ''` (every collection reference has both, so both indexes hold
--   every one).
CREATE INDEX reference_resource_collection_idx ON reference USING btree (resource_id COLLATE "C", timezone('UTC', created_at) DESC, author COLLATE "C", object_path COLLATE "C") INCLUDE (created_at) WHERE kind = 'collection' AND object_path <> '';
-- The viewer's own collections of a link, listed first.
CREATE INDEX reference_collection_author_idx ON reference USING btree (resource_id COLLATE "C", author COLLATE "C", timezone('UTC', created_at) DESC, object_path COLLATE "C") INCLUDE (created_at) WHERE kind = 'collection' AND author <> '';
-- The collection a collection reference's file belongs to (`collections::uri`): a post's version
-- file (`/pub/<ns>/v1/posts/<id>/<leaf>.json`, no further `/`) belongs to its post, any other file
-- is a collection of its own.
CREATE FUNCTION collection_post(object_path text) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
 SELECT CASE WHEN object_path ~ '^/pub/[^/]+/v1/posts/[^/]+/[^/]*\.json$'
   THEN regexp_replace(object_path, '/[^/]*$', '') ELSE object_path END
$$;
-- A collection reference's file within its collection (`collections::file`): a version file's leaf
-- under its post, '' for any other file. With `collection_post` it is the path, less one `/`.
CREATE FUNCTION collection_file(object_path text) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
 SELECT CASE WHEN object_path ~ '^/pub/[^/]+/v1/posts/[^/]+/[^/]*\.json$'
   THEN substring(object_path from '[^/]*$') ELSE '' END
$$;
-- A collection's files by author and collection, its current file first: the greatest instant,
-- then file name in byte order (its files share the post, so their leaves order them as their
-- names do). One probe finds the current file, for `collections::settle`, which keeps that file
-- alone (`collections::current`); search's `in_collection` reads a collection's references by
-- author and post. The instant as the walk's. The path is stored once, as post and leaf, so every
-- path ingest accepts fits a btree entry.
CREATE INDEX reference_collection_post_idx ON reference USING btree (author, collection_post(object_path), timezone('UTC', created_at) DESC, collection_file(object_path) COLLATE "C" DESC) INCLUDE (created_at) WHERE kind = 'collection';

-- References as they count: an alias's count for its root, posts' own rows left out. Each
-- reference is one row: as itself when its link is no alias, else for the alias's one root (the
-- alias is the key of `resource_alias`). The resource is a plain column of each branch, so a
-- probe by resource reads `reference` by resource and `resource_alias` by alias and by root.
CREATE VIEW effective_reference AS
    SELECT f.resource_id, f.author, f.object_path, f.kind, f.label, f.created_at
    FROM reference f
    -- A post's own row keeps its card alive; it references nothing else, so it does not count.
    WHERE f.kind <> 'post' AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id = f.resource_id)
    UNION ALL
    SELECT a.root_id, f.author, f.object_path, f.kind, f.label, f.created_at
    FROM resource_alias a JOIN reference f ON f.resource_id = a.alias_id
    WHERE f.kind <> 'post';

-- Resources a page derives (an event adapter's entries), each with its adapter and when the
-- link lapses.
CREATE TABLE resource_derivation (
    parent_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    child_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    -- The event adapter that derived it: an `id` of `data/event-adapters.json` (a registry, so no
    -- list here).
    adapter text NOT NULL,
    expires_at timestamp with time zone,
    -- The latest instant the parent's feed rows ask for (`sources`' greatest).
    linked_until timestamp with time zone,
    first_seen_at timestamp with time zone NOT NULL DEFAULT now(),
    -- Feed row key -> the instant that row asks for (store::link_sources).
    sources jsonb NOT NULL DEFAULT '{}',
    PRIMARY KEY (parent_id, child_id),
    CONSTRAINT resource_derivation_check CHECK (parent_id <> child_id),
    CONSTRAINT resource_derivation_expires_at_range CHECK (expires_at >= '0001-01-01 00:00:00+00' AND expires_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT resource_derivation_linked_until_range CHECK (linked_until >= '0001-01-01 00:00:00+00' AND linked_until < '10000-01-01 00:00:00+00')
);
CREATE INDEX resource_derivation_adapter_expires_idx ON resource_derivation USING btree (adapter, expires_at);
CREATE INDEX resource_derivation_child_idx ON resource_derivation USING btree (child_id);

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
    -- The classes in `source::ERROR_CLASSES`, exactly.
    CONSTRAINT source_error_class CHECK (last_error IS NULL OR last_error IN ('dns', 'connect', 'timeout', 'http_4xx', 'http_5xx', 'parse', 'policy', 'too_large', 'too_complex'))
);


-- ── Events and places ────────────────────────────────────────────────────────────────────────

-- Whether `versions` is an object of documents each within a document's cap (`osm_place.doc`'s):
-- as many as the retention keeps, none larger than a document.
CREATE FUNCTION osm_versions_fit(versions jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT CASE WHEN jsonb_typeof(versions) = 'object' THEN NOT EXISTS (SELECT 1 FROM jsonb_each(versions) v WHERE octet_length(v.value::text) > 8192) ELSE false END $$;

-- An OpenStreetMap object as Pinakes read it, with each version a resource or event refers to
-- (`versions`). OSM-derived values live only here, in the OSM catalogue's cards
-- (`catalogue_card` rows of catalogue `osm`), `osm_rename`, `osm_removed` and `osm_successor`:
-- the ODbL dump holds them all.
CREATE TABLE osm_place (
    osm_type text NOT NULL CONSTRAINT osm_place_osm_type_check CHECK (osm_type IN ('node', 'way', 'relation')),
    osm_id bigint NOT NULL CONSTRAINT osm_place_osm_id_check CHECK (osm_id > 0),
    point geometry(Point,4326),
    -- The place's four bounding edges (`ResourceMeta.bbox`).
    bbox double precision[] CONSTRAINT osm_place_bbox_check CHECK (array_ndims(bbox) = 1 AND cardinality(bbox) = 4),
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
    -- One normalised document per version, by its md5, each within `doc`'s cap; the retention
    -- (`changes::retain`) drops those nothing refers to.
    versions jsonb NOT NULL DEFAULT '{}' CONSTRAINT osm_place_versions_check CHECK (osm_versions_fit(versions)),
    PRIMARY KEY (osm_type, osm_id)
);
COMMENT ON TABLE osm_place IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';
CREATE INDEX osm_place_category_idx ON osm_place USING btree (category);
CREATE INDEX osm_place_geography_idx ON osm_place USING gist ((point::geography));
CREATE INDEX osm_place_name_idx ON osm_place USING gin (name gin_trgm_ops);
CREATE INDEX osm_place_point_idx ON osm_place USING gist (point);
CREATE INDEX osm_place_search_idx ON osm_place USING gin (search_text);
CREATE INDEX osm_place_type_path_idx ON osm_place USING gin (type_path);

-- A deleted OSM object's successor, found by the OSM catalogue's update on evidence: the same
-- Wikidata item (`wikidata`), or a delete and a create in one changeset with the same name within
-- 50 m (`changeset`). Shared links of the old object become aliases of the new one's link.
CREATE TABLE osm_successor (
    old_type text NOT NULL CONSTRAINT osm_successor_old_type_check CHECK (old_type IN ('node', 'way', 'relation')),
    old_id bigint NOT NULL CONSTRAINT osm_successor_old_id_check CHECK (old_id > 0),
    new_type text NOT NULL CONSTRAINT osm_successor_new_type_check CHECK (new_type IN ('node', 'way', 'relation')),
    new_id bigint NOT NULL CONSTRAINT osm_successor_new_id_check CHECK (new_id > 0),
    basis text NOT NULL CONSTRAINT osm_successor_basis_check CHECK (basis IN ('wikidata', 'changeset')),
    found_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (old_type, old_id)
);

-- An imported OSM object whose name changed to an unrelated one (another business moved in), as
-- the OSM catalogue's update saw it: references made before `changed_at` belong to the place it
-- was. Derivative database, as `osm_place` (in the ODbL dump).
CREATE TABLE osm_rename (
    osm_type text NOT NULL CONSTRAINT osm_rename_osm_type_check CHECK (osm_type IN ('node', 'way', 'relation')),
    osm_id bigint NOT NULL CONSTRAINT osm_rename_osm_id_check CHECK (osm_id > 0),
    changed_at timestamp with time zone NOT NULL,
    old_name text NOT NULL CONSTRAINT osm_rename_old_name_check CHECK (octet_length(old_name) BETWEEN 1 AND 2048),
    old_category text CONSTRAINT osm_rename_old_category_check CHECK (octet_length(old_category) <= 512),
    new_name text NOT NULL CONSTRAINT osm_rename_new_name_check CHECK (octet_length(new_name) BETWEEN 1 AND 2048),
    PRIMARY KEY (osm_type, osm_id, changed_at)
);
COMMENT ON TABLE osm_rename IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';

-- A named OSM object an update of the OSM catalogue removed (deleted upstream): its card's last
-- name, category and point, which the deleted-place rule compares once the card is gone. Gone
-- again when the object comes back. Derivative database, as `osm_place` (in the ODbL dump).
CREATE TABLE osm_removed (
    osm_type text NOT NULL CONSTRAINT osm_removed_osm_type_check CHECK (osm_type IN ('node', 'way', 'relation')),
    osm_id bigint NOT NULL CONSTRAINT osm_removed_osm_id_check CHECK (osm_id > 0),
    name text NOT NULL CONSTRAINT osm_removed_name_check CHECK (octet_length(name) BETWEEN 1 AND 2048),
    category text CONSTRAINT osm_removed_category_check CHECK (octet_length(category) <= 512),
    point geometry(Point,4326) NOT NULL,
    removed_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (osm_type, osm_id)
);
COMMENT ON TABLE osm_removed IS 'Derivative database © OpenStreetMap contributors, licensed under ODbL 1.0: https://opendatacommons.org/licenses/odbl/1-0/';

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
    -- The venue's resource, as `resource.venue_id`.
    venue_id text CONSTRAINT occurrence_venue_id_fkey REFERENCES resource(id),
    status text NOT NULL,
    url text,
    doc jsonb NOT NULL DEFAULT '{}',
    point geometry(Point,4326),
    sequence bigint NOT NULL DEFAULT 0,
    modified_at timestamp with time zone,
    content_hash text NOT NULL DEFAULT '',
    search_text tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(doc->>'description', ''))) STORED,
    PRIMARY KEY (resource_id, uid, recurrence_id),
    CONSTRAINT occurrence_document_cap CHECK (octet_length(doc::text) <= 8192),
    CONSTRAINT occurrence_event_boundary CHECK (event_boundary(doc) AND (coalesce(doc #>> '{venue,location_source}' NOT IN ('geocoded', 'osm'), true) OR point IS NULL)),
    CONSTRAINT occurrence_status CHECK (status IN ('scheduled', 'cancelled', 'postponed', 'rescheduled', 'moved_online', 'superseded')),
    CONSTRAINT occurrence_start_at_range CHECK (start_at >= '0001-01-01 00:00:00+00' AND start_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT occurrence_end_at_range CHECK (end_at >= '0001-01-01 00:00:00+00' AND end_at < '10000-01-01 00:00:00+00'),
    CONSTRAINT occurrence_modified_at_range CHECK (modified_at >= '0001-01-01 00:00:00+00' AND modified_at < '10000-01-01 00:00:00+00')
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
    -- The `fetched_at` of the document whose snapshot was last stored (`ResourceMeta.fetched_at`,
    -- to the millisecond).
    last_snapshot_at timestamp with time zone,
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

-- A podcast show's latest episodes as its feed lists them (`episodes`): replaced whole each time
-- the show's card is saved from a read of its feed, newest first by position. `url` is the
-- episode's page or feed item, `stream` its public enclosure (`model::Stream`), when it has one.
CREATE TABLE podcast_episode (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    position smallint NOT NULL CONSTRAINT podcast_episode_position_check CHECK (position >= 0 AND position < 1000),
    guid text NOT NULL CONSTRAINT podcast_episode_guid_check CHECK (octet_length(guid) <= 2048),
    url text NOT NULL CONSTRAINT podcast_episode_url_check CHECK (octet_length(url) <= 2048),
    name text CONSTRAINT podcast_episode_name_check CHECK (octet_length(name) <= 512),
    image text CONSTRAINT podcast_episode_image_check CHECK (octet_length(image) <= 2048),
    published_at timestamp with time zone,
    stream jsonb CONSTRAINT podcast_episode_stream_check CHECK (octet_length(stream::text) <= 4096),
    PRIMARY KEY (resource_id, position),
    CONSTRAINT podcast_episode_published_at_range CHECK (published_at >= '0001-01-01 00:00:00+00' AND published_at < '10000-01-01 00:00:00+00')
);
-- An episode's link, for the look-up of whether a stored list names a page (`episodes::join_page`).
CREATE INDEX podcast_episode_url_idx ON podcast_episode USING btree (url);
-- A show's stored list as its last read of the feed left it: whether the feed lists older episodes
-- than the list holds (the read ended before the feed did, or the feed has more than are kept).
-- Written and cleared with the list's rows (`episodes::replace`, `episodes::clear`).
CREATE TABLE podcast_episode_list (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    older boolean NOT NULL
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
    changed_at timestamp with time zone NOT NULL DEFAULT clock_timestamp(),
    class text NOT NULL CONSTRAINT resource_change_class_check CHECK (class IN ('created', 'rescheduled', 'relocated', 'cancelled', 'uncancelled', 'renamed', 'details', 'deleted', 'aliased')),
    fields text[] NOT NULL,
    before jsonb,
    after jsonb,
    -- Who wrote the change: the resource's route, an event adapter, a source's name (`sources`),
    -- `alias`, or '' when none is known. Drawn from registries, so no list here.
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
CREATE INDEX resource_change_changed_at_idx ON resource_change USING btree (changed_at);
CREATE INDEX resource_change_before_venue_idx ON resource_change USING btree ((before #>> '{venue,osm_id}'), (before #>> '{venue,osm_version}'));
CREATE INDEX resource_change_osm_idx ON resource_change USING btree (osm_type, osm_id) WHERE osm_type IS NOT NULL;
CREATE INDEX resource_change_resource_idx ON resource_change USING btree (resource_id, seq);

-- Changes as they are read: an OSM change shows the place versions it names, an event change its
-- venue's location.
CREATE VIEW change_document AS
    SELECT c.seq, c.resource_id, c.occurrence_key, c.changed_at, c.class, c.fields, c.source,
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

-- The units search ranks: each resource that is not an alias (an event only with a start) and
-- each live occurrence of a feed that is not missing.
CREATE VIEW search_unit AS
    SELECT r.*, r.id AS unit_id, NULL::jsonb AS occurrence
    FROM resource_document r
    WHERE (NOT coalesce(r.type_path @> ARRAY['Event'], false) OR r.start_at IS NOT NULL)
        AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id = r.id)
UNION ALL
    SELECT r.id, r.uri, r.host, o.doc->>'type' AS type, ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')) AS type_path,
        o.name, NULL::text AS category, o.effective_point AS point, o.start_at, o.end_at, o.venue_id, o.document AS doc,
        o.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, NULL::text AS osm_type, NULL::bigint AS osm_id,
        md5(jsonb_build_array(r.id, o.uid, o.recurrence_id)::text) AS unit_id,
        jsonb_build_object('uid', o.uid, 'recurrence_id', nullif(o.recurrence_id, ''), 'status', o.status) AS occurrence
    FROM event_occurrence o JOIN resource r ON r.id = o.resource_id
    WHERE r.status <> 'missing';

-- A resource's own search unit: `search_unit`'s first branch, without occurrences, so `related`
-- reads each related resource's card by its ID alone.
CREATE VIEW resource_unit AS
    SELECT r.*, r.id AS unit_id, NULL::jsonb AS occurrence
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


-- ── The text index's queue ───────────────────────────────────────────────────────────────────

-- The resources whose search units the text index (`search_index`) must read again: one row per
-- write, with the writing transaction's ID. The index reads the rows of every transaction that
-- has finished since its last pass (`pg_snapshot_xmin`), so a write that commits late is never
-- skipped, and deletes the rows its last commit covers.
CREATE TABLE search_index_queue (
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    xid xid8 NOT NULL DEFAULT pg_current_xact_id(),
    resource_id text NOT NULL
);
CREATE INDEX search_index_queue_xid_idx ON search_index_queue USING btree (xid);

-- This database, to the text index: a token made with the database, which the index records and
-- checks with the database's own ID (`pg_database.oid`), so an index built from another database
-- is never opened as this one's.
CREATE TABLE search_index_database (
    id integer PRIMARY KEY CONSTRAINT search_index_database_id_check CHECK (id = 1),
    token uuid NOT NULL DEFAULT gen_random_uuid()
);
INSERT INTO search_index_database(id) SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM search_index_database);

-- A row's resource, or the resource of a row that names one (`resource_id`), on any change.
CREATE FUNCTION queue_search_index() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    -- Each branch is planned only when it runs: `resource` has `id`, the others `resource_id`.
    IF TG_TABLE_NAME = 'resource' THEN
        INSERT INTO search_index_queue(resource_id) VALUES (CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END);
    ELSE
        IF TG_OP <> 'INSERT' THEN INSERT INTO search_index_queue(resource_id) VALUES (OLD.resource_id); END IF;
        IF TG_OP <> 'DELETE' AND (TG_OP = 'INSERT' OR NEW.resource_id IS DISTINCT FROM OLD.resource_id) THEN
            INSERT INTO search_index_queue(resource_id) VALUES (NEW.resource_id);
        END IF;
    END IF;
    RETURN NULL;
END $$;

-- An alias and its root: the alias leaves search, and its references count for the root.
CREATE FUNCTION queue_search_index_alias() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    IF TG_OP <> 'INSERT' THEN
        INSERT INTO search_index_queue(resource_id) VALUES (OLD.alias_id), (OLD.root_id);
    END IF;
    IF TG_OP <> 'DELETE' THEN
        INSERT INTO search_index_queue(resource_id) VALUES (NEW.alias_id), (NEW.root_id);
    END IF;
    RETURN NULL;
END $$;

-- Every resource, when a table a unit's tags or score come from is emptied (a feed reset).
CREATE FUNCTION queue_search_index_all() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    INSERT INTO search_index_queue(resource_id) SELECT id FROM resource;
    RETURN NULL;
END $$;

-- An OSM place's resources, and the events and occurrences held at it as their venue.
CREATE FUNCTION queue_search_index_osm() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    place record;
BEGIN
    IF TG_OP = 'DELETE' THEN place := OLD; ELSE place := NEW; END IF;
    INSERT INTO search_index_queue(resource_id)
        SELECT id FROM resource WHERE osm_type = place.osm_type AND osm_id = place.osm_id
        UNION SELECT id FROM resource WHERE doc #>> '{venue,osm_id}' = place.osm_type || '/' || place.osm_id
        UNION SELECT resource_id FROM occurrence WHERE doc #>> '{venue,osm_id}' = place.osm_type || '/' || place.osm_id;
    RETURN NULL;
END $$;

CREATE TRIGGER resource_search_index_insert AFTER INSERT OR DELETE ON resource FOR EACH ROW EXECUTE FUNCTION queue_search_index();
-- `point` is compared as bytes here and below: a geometry's `=` does not resolve under the empty
-- search path `pg_restore` sets, so with it a dump of these two triggers would not restore.
CREATE TRIGGER resource_search_index_update AFTER UPDATE OF uri, host, resolver, doc, status, name, type, type_path, category, point, start_at, end_at, venue_id, osm_type, osm_id, osm_version ON resource FOR EACH ROW
    WHEN ((OLD.uri, OLD.host, OLD.resolver, OLD.doc, OLD.status, OLD.name, OLD.type, OLD.type_path, OLD.category, OLD.point::bytea, OLD.start_at, OLD.end_at, OLD.venue_id, OLD.osm_type, OLD.osm_id, OLD.osm_version)
        IS DISTINCT FROM (NEW.uri, NEW.host, NEW.resolver, NEW.doc, NEW.status, NEW.name, NEW.type, NEW.type_path, NEW.category, NEW.point::bytea, NEW.start_at, NEW.end_at, NEW.venue_id, NEW.osm_type, NEW.osm_id, NEW.osm_version))
    EXECUTE FUNCTION queue_search_index();
CREATE TRIGGER reference_search_index AFTER INSERT OR DELETE OR UPDATE ON reference FOR EACH ROW EXECUTE FUNCTION queue_search_index();
CREATE TRIGGER score_search_index AFTER INSERT OR DELETE OR UPDATE ON resource_score FOR EACH ROW EXECUTE FUNCTION queue_search_index();
CREATE TRIGGER occurrence_search_index AFTER INSERT OR DELETE OR UPDATE ON occurrence FOR EACH ROW EXECUTE FUNCTION queue_search_index();
CREATE TRIGGER alias_search_index AFTER INSERT OR DELETE OR UPDATE ON resource_alias FOR EACH ROW EXECUTE FUNCTION queue_search_index_alias();
CREATE TRIGGER osm_search_index_insert AFTER INSERT OR DELETE ON osm_place FOR EACH ROW EXECUTE FUNCTION queue_search_index_osm();
CREATE TRIGGER osm_search_index_update AFTER UPDATE OF doc, status, name, display_name, type, type_path, category, point, versions ON osm_place FOR EACH ROW
    WHEN ((OLD.doc, OLD.status, OLD.name, OLD.display_name, OLD.type, OLD.type_path, OLD.category, OLD.point::bytea, OLD.versions)
        IS DISTINCT FROM (NEW.doc, NEW.status, NEW.name, NEW.display_name, NEW.type, NEW.type_path, NEW.category, NEW.point::bytea, NEW.versions))
    EXECUTE FUNCTION queue_search_index_osm();
CREATE TRIGGER reference_search_index_truncate AFTER TRUNCATE ON reference FOR EACH STATEMENT EXECUTE FUNCTION queue_search_index_all();
CREATE TRIGGER score_search_index_truncate AFTER TRUNCATE ON resource_score FOR EACH STATEMENT EXECUTE FUNCTION queue_search_index_all();


-- ── Groups ───────────────────────────────────────────────────────────────────────────────────

-- A group: units that are the same thing (see docs/design.md). `curated` groups are kept when
-- they have no members.
CREATE TABLE "group" (
    id text PRIMARY KEY CONSTRAINT group_id_check CHECK (id ~ '^[0-9a-f]{32}$'),
    curated boolean NOT NULL DEFAULT false
);

-- A unit (a resource or an occurrence) and its group, if any.
CREATE TABLE group_unit (
    unit_id text PRIMARY KEY CONSTRAINT group_unit_unit_id_check CHECK (unit_id ~ '^[0-9a-f]{32}$'),
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    occurrence_key text NOT NULL DEFAULT '',
    group_id text REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED,
    occurrence jsonb,
    admitted_at timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX group_unit_group_idx ON group_unit USING btree (group_id, resource_id, unit_id);
CREATE INDEX group_unit_resource_idx ON group_unit USING btree (resource_id);

-- The grouping keys (evidence) of each unit. The unit's resource and group are its `group_unit`
-- row's, read by the unit.
CREATE TABLE resource_group (
    unit_id text NOT NULL REFERENCES group_unit(unit_id) ON DELETE CASCADE,
    key text NOT NULL CONSTRAINT resource_group_key_check CHECK (octet_length(key) <= 2200),
    admitted_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (unit_id, key)
);
CREATE INDEX resource_group_admission_idx ON resource_group USING btree (key, admitted_at, unit_id);

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
    -- Why the last run was deferred (`groups`; a test holds the list equal to what the code writes).
    last_error text CONSTRAINT group_dirty_last_error_check CHECK (last_error IN ('work_budget', 'lock_busy', 'snapshot_changed', 'group_failed')),
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

-- A group member's document, as its card shows it: a resource's or an occurrence's.
CREATE VIEW group_member_document AS
    SELECT r.*, u.unit_id, u.occurrence
    FROM group_unit u
    CROSS JOIN LATERAL (SELECT * FROM resource_document WHERE resource_document.id = u.resource_id OFFSET 0) r
    WHERE u.occurrence IS NULL
UNION ALL
    SELECT r.id, r.uri, r.host, o.doc->>'type' AS type, ARRAY(SELECT jsonb_array_elements_text(o.doc->'type_path')) AS type_path,
        o.name, NULL::text AS category, o.effective_point AS point, o.start_at, o.end_at, o.venue_id, o.document AS doc,
        o.search_text, r.resolver, r.resolved_at, r.refresh_at, r.status, NULL::text AS osm_type, NULL::bigint AS osm_id,
        u.unit_id, u.occurrence
    FROM group_unit u
    CROSS JOIN LATERAL (SELECT * FROM event_occurrence
        WHERE event_occurrence.resource_id = u.resource_id AND event_occurrence.uid = u.occurrence->>'uid'
            AND event_occurrence.recurrence_id = coalesce(u.occurrence->>'recurrence_id', '') OFFSET 0) o
    JOIN resource r ON r.id = u.resource_id
    WHERE u.occurrence IS NOT NULL AND r.status <> 'missing';

-- A change grouping reads marks the resource dirty and bumps its `group_revision`; grouping's
-- fence (groups::fence) relies on the bump. A catalogue card's labels and version are no change
-- to what grouping reads. A child row's change locks the resource's row FOR NO KEY UPDATE before
-- it marks it (the bump after it takes that lock anyway): it waits for a save holding the row,
-- which marks the resource itself, instead of marking it first and then waiting for the save.
CREATE FUNCTION mark_group_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings'-'catalogue'-'labels') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings'-'catalogue'-'labels') THEN RETURN NULL; END IF;
  INSERT INTO group_dirty(resource_id) VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  PERFORM 1 FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END FOR NO KEY UPDATE;
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
-- ID order) FOR NO KEY UPDATE: it waits for a save of a member (its row, then its mark) before it
-- marks, never after.
CREATE FUNCTION group_member_deleted() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 PERFORM 1 FROM resource WHERE id IN(SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id) ORDER BY id FOR NO KEY UPDATE;
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
-- In byte order, where the keys of one catalogue (`catalogue:<name>:…`) are one range, then by
-- resource: the start's sweep reads only those of the catalogues it removes, a page at a time from
-- where the last ended, also inside one key many links hold.
CREATE INDEX resource_key_catalogue_idx ON resource_key USING btree (key COLLATE "C", resource_id) WHERE source = 'catalogue';

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
    -- A time that belongs to the link (an archived copy's capture), its `at` in `also_on`.
    captured_at timestamp with time zone,
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

-- Where the links of a recording's or an album's card sit among the releases MusicBrainz lists
-- (the `editions` source), for the card `resource_id`: `rank` 0 for a link on the card's own
-- release, then 1, 2, … for one on the original edition, its releases by date; `release` is that
-- release. Rank 1000 with no `release`: its service says it is on another release (a later
-- edition, a compilation). A link with no row is unplaced, after the placed ones and before those
-- on another release. `also_on()` shows each service's best link.
CREATE TABLE link_release (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    link_id text NOT NULL,
    release uuid,
    rank smallint NOT NULL,
    source text NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    PRIMARY KEY (resource_id, link_id),
    CONSTRAINT link_release_link_id_check CHECK ((link_id ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT link_release_rank_check CHECK ((rank BETWEEN 0 AND 1000)),
    CONSTRAINT link_release_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$'))
);

-- The also-on sources the operator has switched off (`[also_on.sources.<id>] enabled = false`,
-- or off by default and not turned on), written at startup from the configuration
-- (`also_on::configure`). Their keys, facts and links stay and expire as before, but nothing
-- reads them: not a card, its `also_on`, a search or grouping. Switched on again, they show.
CREATE TABLE also_on_source_off (
    source text PRIMARY KEY,
    CONSTRAINT also_on_source_off_source_check CHECK ((source ~ '^[a-z0-9_]{1,32}$'))
);

-- Moves when the cards that hold a switched-off source's finding (`hinted_by`) and that no refresh
-- reads may have changed: the set of switched-off sources changed (`also_on::configure`), or such a
-- card became final or dead-lettered (the trigger below). `also_on::unhint` reads those cards only
-- when this has moved since the run it records in `also_on_unhint_done`.
CREATE SEQUENCE also_on_unhint_due;
-- The count of `also_on_unhint_due` (its value, plus one once called) the last completed
-- `also_on::unhint` run covered; 0 before any.
CREATE SEQUENCE also_on_unhint_done MINVALUE 0 START WITH 0;
CREATE FUNCTION mark_also_on_unhint_due() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
 IF jsonb_typeof(NEW.doc->'hinted_by')='object'
    AND EXISTS (SELECT 1 FROM also_on_source_off o WHERE NEW.doc->'hinted_by' ? o.source) THEN
  PERFORM nextval('also_on_unhint_due');
 END IF;
 RETURN NULL;
END
$$;
CREATE TRIGGER resource_also_on_unhint_due AFTER UPDATE OF final, dead_letter ON resource FOR EACH ROW
    WHEN ((NEW.final AND NOT OLD.final) OR (NEW.dead_letter AND NOT OLD.dead_letter))
    EXECUTE FUNCTION mark_also_on_unhint_due();

-- Resources whose sources are due: after a save that changed the document, and when the
-- shortest source lifetime has passed. Failures back off like grouping's.
CREATE TABLE also_on_dirty (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    -- The class of the last failed run (`resolver::error_class`), as `resource.last_error`.
    last_error text
);
CREATE INDEX also_on_dirty_due_idx ON also_on_dirty USING btree (next_attempt_at, resource_id);

-- A saved document (not a refetch that changed nothing, nor a catalogue card's new labels or
-- content hash alone) makes its sources due.
CREATE FUNCTION mark_also_on_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 IF TG_OP='UPDATE' AND (NEW.doc-'fetched_at'-'ttl_secs'-'warnings'-'catalogue'-'labels') IS NOT DISTINCT FROM (OLD.doc-'fetched_at'-'ttl_secs'-'warnings'-'catalogue'-'labels') AND NEW.status IS NOT DISTINCT FROM OLD.status THEN RETURN NULL; END IF;
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
CREATE TRIGGER link_release_revision BEFORE INSERT OR DELETE OR UPDATE ON link_release FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

-- A keyed derived link is evidence for the tagged resource it names: a change regroups it. The
-- named rows are locked first (in ID order), so a save holding one is waited for before the mark,
-- as `mark_group_dirty` does.
CREATE FUNCTION mark_named_group_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ DECLARE named text[];
BEGIN
 IF TG_OP='UPDATE' AND OLD.key IS NOT DISTINCT FROM NEW.key AND OLD.link_id=NEW.link_id THEN RETURN NULL; END IF;
 named := ARRAY[CASE WHEN TG_OP<>'INSERT' AND OLD.key IS NOT NULL THEN OLD.link_id END,
                CASE WHEN TG_OP<>'DELETE' AND NEW.key IS NOT NULL THEN NEW.link_id END];
 PERFORM 1 FROM resource WHERE id=ANY(named) ORDER BY id FOR NO KEY UPDATE;
 INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE id=ANY(named) ON CONFLICT DO NOTHING;
 UPDATE resource SET group_revision=group_revision+1 WHERE id=ANY(named);
 RETURN NULL;
END $$;
CREATE TRIGGER derived_link_named_group_dirty AFTER INSERT OR DELETE OR UPDATE ON derived_link FOR EACH ROW EXECUTE FUNCTION mark_named_group_dirty();

-- A source switched off or on changes what grouping and the same-recording pass read: the
-- resources its keys and keyed links name are regrouped (their `group_revision` bumped, as a
-- key's change does), and its recordings' pairs are read again. Searches see a new revision.
CREATE FUNCTION also_on_source_switched() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ DECLARE src text := CASE WHEN TG_OP='DELETE' THEN OLD.source ELSE NEW.source END;
BEGIN
 WITH named AS (SELECT resource_id AS id FROM resource_key WHERE source=src
     UNION SELECT link_id FROM derived_link WHERE source=src AND key IS NOT NULL),
  bumped AS (UPDATE resource r SET group_revision=r.group_revision+1 FROM named n WHERE r.id=n.id RETURNING r.id)
 INSERT INTO group_dirty(resource_id) SELECT id FROM bumped ON CONFLICT DO NOTHING;
 INSERT INTO same_recording_dirty(resource_id)
  SELECT DISTINCT f.resource_id FROM resource_fact f JOIN resource r ON r.id=f.resource_id
  WHERE f.source=src AND f.name IN ('artist','duration') AND r.type_path @> ARRAY['MusicRecording']
  ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at WHERE same_recording_dirty.next_attempt_at>excluded.next_attempt_at;
 RETURN NULL;
END $$;
CREATE TRIGGER also_on_source_switched AFTER INSERT OR DELETE ON also_on_source_off FOR EACH ROW EXECUTE FUNCTION also_on_source_switched();
CREATE TRIGGER also_on_source_revision BEFORE INSERT OR DELETE ON also_on_source_off FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

-- A card's `also_on`: the other members of its group (tagged links, with their ID: other
-- resources first, one each by ID, then other units of its own resource), then the live derived
-- links of the card's resource and of every member, by service name, each URL once and never
-- the card itself; at most `cap` entries. `resource` is the card's resource ID for a resource's
-- card, and the unit for an occurrence (whose feed's links are not its own). `members` false
-- (groups switched off) leaves the group out. Read at request time: OSM-derived values never
-- enter these tables, and a switched-off source's rows (`also_on_source_off`) are not read.
-- A card (`per_site`) shows one entry per service: the link on the card's own release first, then
-- one on the original edition (`link_release`, best rank); among links neither places, a tagged
-- member whose URL is on a service (`doc.service`: a music service, or one a derived link names,
-- such as Wikidata's F-Droid link) ahead of the derived links (on music, the member on the
-- recording's original album first), then the derived link most members name; then a page ahead
-- of a file (a `.pdf`), the shorter link, the link (a link its service puts on another release
-- last). A page that only calls itself after a service
-- (its own `site`) takes no service's place, and other tagged members show one entry each. The
-- card's own service, by its URL, has its link, the card, so it shows no other page there.
-- `per_site` false lists them all.
CREATE FUNCTION also_on(unit text, resource text, cap integer, members boolean, per_site boolean DEFAULT true) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 WITH grp AS (SELECT group_id,resource_id FROM group_unit WHERE members AND unit_id=unit AND group_id IS NOT NULL),
 -- Before any document is read: one unit for each of up to `cap` other resources (those the card
 -- places first, then by ID), then
 -- up to `cap` more units of the card's own resource (a feed's other occurrences), and the
 -- unit itself. A resource with many units never crowds the others out.
 -- The members the card places come first (`link_release`), so the cap never cuts one on its own
 -- or the original release.
 others AS (SELECT unit_id FROM (SELECT DISTINCT ON (u.resource_id) u.unit_id,u.resource_id FROM grp JOIN group_unit u USING(group_id)
     WHERE u.resource_id<>grp.resource_id ORDER BY u.resource_id,u.unit_id) d
   ORDER BY coalesce((SELECT min(x.rank) FROM link_release x WHERE x.resource_id=also_on.resource AND x.link_id=d.resource_id
     AND x.expires_at>now() AND x.source NOT IN (SELECT source FROM also_on_source_off)),500),d.resource_id LIMIT cap),
 siblings AS (SELECT u.unit_id FROM grp JOIN group_unit u USING(group_id,resource_id)
   WHERE u.unit_id<>unit ORDER BY u.unit_id LIMIT cap),
 picked AS (SELECT unit_id FROM others UNION SELECT unit_id FROM siblings UNION SELECT unit FROM grp),
 m AS (SELECT d.id,CASE WHEN d.occurrence IS NULL THEN d.uri ELSE coalesce(nullif(d.doc->>'url',''),d.uri) END AS uri,
   d.host,d.doc->>'site' AS site,d.doc->>'service' AS service,d.osm_type IS NOT NULL AS osm,d.unit_id=unit AS own,d.id=(SELECT resource_id FROM grp) AS mine,
   d.type IN ('MusicRecording','MusicAlbum','MusicGroup','MusicPlaylist') AS music,
   (SELECT f.value FROM resource_fact f WHERE f.resource_id=d.id AND f.name='album' AND f.expires_at>now()
     AND f.source NOT IN (SELECT source FROM also_on_source_off) ORDER BY f.value LIMIT 1) AS album
   FROM picked p
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=p.unit_id OFFSET 0) d),
 -- The service a member's URL is on: the one its save recorded (`doc.service`) or an OSM object's
 -- OpenStreetMap. Never what a page says about itself (its `site`, its type).
 tagged AS (SELECT id,uri,min(host) AS host,min(site) AS site,
     coalesce(min(service),CASE WHEN bool_or(osm) THEN 'OpenStreetMap' END) AS service,
     bool_or(mine) AS mine,bool_or(music) AS music,min(album) AS album FROM m GROUP BY id,uri HAVING NOT bool_or(own)),
 owners AS (SELECT resource AS resource_id UNION SELECT u.resource_id FROM grp JOIN group_unit u USING(group_id)),
 original AS (SELECT lower(min(f.value)) AS title FROM resource_fact f JOIN owners o USING(resource_id) WHERE f.name='original_album' AND f.expires_at>now()
   AND f.source NOT IN (SELECT source FROM also_on_source_off)),
 -- The services the live derived links name, before those already tagged members are dropped:
 -- two tagged spellings of one F-Droid page share its slot even when the derived link is one of them.
 services AS (SELECT DISTINCT lower(l.site) AS service FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND l.site IS NOT NULL AND l.source NOT IN (SELECT source FROM also_on_source_off)),
 derived AS (SELECT DISTINCT ON (l.link_id) l.link_id,l.uri,l.site,l.via,l.kind,l.captured_at,
     count(*) OVER (PARTITION BY l.link_id) AS named
   FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND l.source NOT IN (SELECT source FROM also_on_source_off)
     AND NOT EXISTS(SELECT 1 FROM owners x WHERE x.resource_id=l.link_id)
     AND NOT EXISTS(SELECT 1 FROM tagged t WHERE t.id=l.link_id)
   ORDER BY l.link_id,l.seen_at DESC,l.via),
 -- Where each link sits among the card's releases (`link_release`): NULL when unplaced.
 placed AS (SELECT x.link_id,min(x.rank) AS rank FROM link_release x WHERE x.resource_id=also_on.resource
   AND x.expires_at>now() AND x.source NOT IN (SELECT source FROM also_on_source_off) GROUP BY x.link_id),
 entries AS (SELECT CASE WHEN mine THEN 1 ELSE 0 END AS o,id AS k1,uri AS k2,
     p.rank AS edition,
     CASE WHEN starts_with(lower(album),(SELECT title FROM original)) THEN -1 ELSE 0 END::bigint AS rank,
     CASE WHEN service IS NOT NULL AND (music OR lower(service) IN (SELECT service FROM services)) THEN lower(service) ELSE 'member:'||id END AS service,
     jsonb_build_object('id',id,'uri',uri,'host',host,'site',site) AS e FROM tagged LEFT JOIN placed p ON p.link_id=tagged.id
   UNION ALL SELECT 2,lower(site),uri,p.rank,-named,lower(site),jsonb_strip_nulls(jsonb_build_object('uri',uri,'site',site,'derived',true,'via',via,'kind',kind,'at',captured_at)) FROM derived LEFT JOIN placed p ON p.link_id=derived.link_id),
 -- The card's own service, by its URL alone, like a member's.
 own AS (SELECT lower(coalesce(r.doc->>'service',CASE WHEN r.osm_type IS NOT NULL THEN 'OpenStreetMap' END)) AS service,lower(r.host) AS host,
   r.type IN ('MusicRecording','MusicAlbum','MusicGroup','MusicPlaylist') AS music FROM resource r WHERE r.id=also_on.resource),
 shown AS (SELECT * FROM (SELECT *,row_number() OVER (PARTITION BY service ORDER BY coalesce(edition,500),o,rank,
     e->>'kind' IS NOT DISTINCT FROM 'pdf' OR lower(right(k2,4))='.pdf',
     length(k2),k2) AS nth FROM entries) x
   WHERE NOT per_site OR (nth=1 AND service IS DISTINCT FROM (SELECT service FROM own)
     -- Nor, on a music card, a page of its own service whose card names no service yet: one on
     -- its host, before the cap counts it.
     AND NOT coalesce((SELECT music AND service IS NOT NULL FROM own)
       AND lower(split_part(split_part(k2,'://',2),'/',1))=(SELECT host FROM own),false)))
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
-- to refill its list. `scan_after` and `scan_after_start` are the position where a read that
-- examined its most candidates (hints::SCAN) with room left stopped: the id and the start, as the
-- start index orders the units, round those starting within 50 hours of it, so the next read goes
-- on from there (NULL once a read ends with its list full or the candidates run out). Pairs are
-- matched in this table, through its start index (`same_event_unit_start_idx`, in that order).
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
    tz text,
    scan_after text,
    scan_after_start timestamp with time zone
);
CREATE INDEX same_event_unit_resource_idx ON same_event_unit USING btree (resource_id);
CREATE INDEX same_event_unit_start_idx ON same_event_unit USING btree (start_at, unit_id);

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

-- Units a refill passed over because another transaction held them (`same_event::refill`): read
-- again as a refill whatever their fingerprint. A read takes the marks made before it began.
CREATE TABLE same_event_refill (
    unit_id text PRIMARY KEY,
    marked_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL
);

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


-- ── The same-recording hint ──────────────────────────────────────────────────────────────────

-- Recordings (`MusicRecording` roots) on different pages that are possibly the same one when
-- nothing exact joins them: the same title and first artist as two cards compare them
-- (matching.rs) and a length within 3 s. A hint only: never a group, never counted, never
-- ranks. A background pass (same_recording::drain) writes the pairs; cards only read them.

-- A recording the pass has read, with what pairs are matched on: its title and artist keys and
-- its length; and the `fingerprint` of the values they came from, so a change is read again.
-- `refill:{fingerprint}` marks one whose list lost a pair to another's change: it is read again
-- to refill its list. `scan_after` and `scan_after_length` are the position where a read that
-- examined its most candidates (hints::SCAN) with room left stopped: the id and the length, as
-- the index orders candidates, of the one it examined last, so the next read goes on from that
-- position (NULL once a read ends with its list full or the candidates run out).
CREATE TABLE same_recording_unit (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    fingerprint text NOT NULL,
    title_key text NOT NULL,
    artist_key text NOT NULL,
    length_secs integer,
    scan_after text,
    scan_after_length integer
);
-- In (title, artist, length, id) order, so a recording's candidates are read from the length
-- window (or the lengths unknown) and the read stops at the pairs kept, or at the candidates it
-- may examine.
CREATE INDEX same_recording_unit_name_idx ON same_recording_unit USING btree (title_key, artist_key, length_secs, resource_id);

-- One row per direction of a pair: `score` is 1 when both lengths are known and within 3 s,
-- 0.7 when one is unknown. A recording names at most 20 and is named by at most 40.
CREATE TABLE same_recording (
    resource_id text NOT NULL REFERENCES same_recording_unit(resource_id) ON DELETE CASCADE,
    other_id text NOT NULL REFERENCES same_recording_unit(resource_id) ON DELETE CASCADE,
    score real NOT NULL,
    PRIMARY KEY (resource_id, other_id),
    CONSTRAINT same_recording_pair CHECK (resource_id <> other_id)
);
CREATE INDEX same_recording_other_idx ON same_recording USING btree (other_id);

-- Resources whose recording may have changed (its name, type, status, or the artist and
-- length its page or the sources state), whose list lost a pair, or that the sweep found unread.
CREATE TABLE same_recording_dirty (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE INDEX same_recording_dirty_due_idx ON same_recording_dirty USING btree (next_attempt_at, resource_id);

-- Recordings a refill passed over because another transaction held their unit
-- (`same_recording::refill`): read again as a refill whatever their fingerprint. A read takes the
-- marks made before it began.
CREATE TABLE same_recording_refill (
    resource_id text PRIMARY KEY,
    marked_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL
);

-- Changes that can move a pair: a recording's (or a former recording's) name, type, status,
-- page artist or duration, and an artist or duration fact of a recording: added, deleted, its
-- value changed, or valid again after it had expired (its expiry extended). A plain refresh of a
-- valid fact marks nothing.
CREATE FUNCTION mark_same_recording_dirty() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE target text;
BEGIN
 IF TG_TABLE_NAME='resource' THEN
  IF NOT coalesce(NEW.type_path @> ARRAY['MusicRecording'],false)
     AND NOT (TG_OP='UPDATE' AND coalesce(OLD.type_path @> ARRAY['MusicRecording'],false)) THEN RETURN NULL; END IF;
  IF TG_OP='UPDATE' AND (NEW.name,NEW.type_path,NEW.status,NEW.doc#>>'{attributes,artist}',NEW.doc#>>'{attributes,duration}')
     IS NOT DISTINCT FROM (OLD.name,OLD.type_path,OLD.status,OLD.doc#>>'{attributes,artist}',OLD.doc#>>'{attributes,duration}') THEN RETURN NULL; END IF;
  target:=NEW.id;
 ELSE
  IF TG_OP='UPDATE' AND NEW.value IS NOT DISTINCT FROM OLD.value
     AND NOT (OLD.expires_at<=now() AND NEW.expires_at>now()) THEN RETURN NULL; END IF;
  IF TG_OP='DELETE' THEN target:=OLD.resource_id; ELSE target:=NEW.resource_id; END IF;
  IF coalesce(NEW.name,OLD.name) NOT IN ('artist','duration') THEN RETURN NULL; END IF;
  IF NOT EXISTS (SELECT 1 FROM resource WHERE id=target AND type_path @> ARRAY['MusicRecording']) THEN RETURN NULL; END IF;
 END IF;
 INSERT INTO same_recording_dirty(resource_id) VALUES(target)
  ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at WHERE same_recording_dirty.next_attempt_at>excluded.next_attempt_at;
 RETURN NULL;
END $$;
CREATE TRIGGER same_recording_resource_dirty AFTER INSERT OR UPDATE OF name, type_path, status, doc ON resource FOR EACH ROW EXECUTE FUNCTION mark_same_recording_dirty();
CREATE TRIGGER same_recording_fact_dirty AFTER INSERT OR DELETE OR UPDATE ON resource_fact FOR EACH ROW EXECUTE FUNCTION mark_same_recording_dirty();

-- The hint on a recording's card: up to `n` recordings that are possibly the same one, best
-- first, each as {id, uri, host, site, start (null), score}, the shape of the event hint. A
-- member of the card's own group, an alias and a card that is not ok are never listed. Read at
-- request time.
CREATE FUNCTION same_recording_hint(me text, n integer) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT coalesce((
  SELECT jsonb_agg(jsonb_build_object('id',x.id,'uri',x.uri,'host',x.host,'site',x.site,'start',NULL,'score',x.score) ORDER BY x.score DESC,x.uri)
  FROM (SELECT r.id,r.uri,r.host,r.doc->>'site' AS site,round(p.score::numeric,2) AS score
   FROM same_recording p JOIN resource r ON r.id=p.other_id
   WHERE p.resource_id=me AND r.status='ok'
   AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id)
   AND NOT EXISTS (SELECT 1 FROM group_unit a JOIN group_unit b ON b.group_id=a.group_id WHERE a.unit_id=me AND b.unit_id=p.other_id AND a.group_id IS NOT NULL)
   ORDER BY p.score DESC,r.uri LIMIT n) x),'[]'::jsonb)
$$;

-- A card's hint by its kind: an event's with `events` on, a recording's with `recordings` on;
-- NULL for another card, or a kind whose hint is off.
CREATE FUNCTION maybe_same_hint(unit text, type_path text[], n integer, events boolean, recordings boolean) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT CASE WHEN events AND coalesce(type_path @> ARRAY['Event'],false) THEN same_event_hint(unit,type_path,n)
             WHEN recordings AND coalesce(type_path @> ARRAY['MusicRecording'],false) THEN same_recording_hint(unit,n) END
$$;


-- ── Catalogues ───────────────────────────────────────────────────────────────────────────────

-- The catalogues: `Catalogue::ALL`'s names (a test holds the two lists equal). Every table with a
-- `catalogue` column checks it with this.
CREATE FUNCTION is_catalogue(name text) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT name IN ('wikidata', 'musicbrainz', 'osm', 'openalex', 'podcastindex', 'tranco', 'software') $$;

-- One item a catalogue import holds (`catalogue`): Wikidata, MusicBrainz, OpenStreetMap, OpenAlex,
-- Podcast Index, Tranco or software packages. The catalogue's importer is its only writer; the
-- fetch queue, the refresh times and the change journal never touch these rows. `source` is the
-- sub-source (`crates.io`, `f-droid`, `flathub` for software; the catalogue's name otherwise).
-- `uri` is the item's canonical link, normalized as a referenced link is (`model::identity`);
-- `keys` are join keys in the forms resources carry (`wikidata:Q42`, `mb:recording:<mbid>`,
-- `isrc:…`, `osm:node/1`, `doi:…`) and `url:<resource id>` for each link people share for the
-- same item; `related_uris` are links that belong to the item's group without being the item
-- (an official website). `labels` are valid Pubky tag labels. `fame` is 0–1. A full import
-- stamps every row it reads with its generation (`seen_generation`) and then removes the rows it
-- did not read; a row changes only when its `content_hash` does, and only such a change fires the
-- row triggers below (a stamp fires none).
CREATE TABLE catalogue_card (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    catalogue text NOT NULL CONSTRAINT catalogue_card_catalogue_check CHECK (is_catalogue(catalogue)),
    source text NOT NULL CONSTRAINT catalogue_card_source_check CHECK (source ~ '^[a-z0-9][a-z0-9._-]{0,31}$'),
    source_id text NOT NULL CONSTRAINT catalogue_card_source_id_check CHECK (octet_length(source_id) BETWEEN 1 AND 512),
    uri text NOT NULL CONSTRAINT catalogue_card_uri_check CHECK (octet_length(uri) BETWEEN 1 AND 2048),
    type text NOT NULL,
    name text NOT NULL,
    other_names text[] NOT NULL DEFAULT '{}',
    description text,
    labels text[] NOT NULL DEFAULT '{}',
    keys text[] NOT NULL DEFAULT '{}',
    related_uris text[] NOT NULL DEFAULT '{}',
    point geometry(Point,4326),
    fame real NOT NULL DEFAULT 0 CONSTRAINT catalogue_card_fame_check CHECK (fame >= 0 AND fame <= 1),
    details jsonb NOT NULL DEFAULT '{}' CONSTRAINT catalogue_card_details_check CHECK (jsonb_typeof(details) = 'object' AND octet_length(details::text) <= 8192),
    content_hash bytea NOT NULL CONSTRAINT catalogue_card_content_hash_check CHECK (octet_length(content_hash) = 32),
    seen_generation bigint NOT NULL,
    updated_at timestamp with time zone NOT NULL DEFAULT now(),
    CONSTRAINT catalogue_card_source_id_key UNIQUE (catalogue, source_id)
)
-- A fifth of each page free: a full import's stamp of an unchanged card (`seen_generation`, in no
-- index) that finds room on its page is a heap-only update, with no new index entries (about one
-- stamp in four when a whole catalogue is stamped).
WITH (fillfactor = 80);
-- Searches without text read each catalogue that is on in fame order, by type and by host too, so
-- the cards of one switched off but not dropped are never read.
CREATE INDEX catalogue_card_fame_idx ON catalogue_card USING btree (catalogue, fame DESC, id);
CREATE INDEX catalogue_card_keys_idx ON catalogue_card USING gin (keys);
CREATE INDEX catalogue_card_labels_idx ON catalogue_card USING gin (labels);
-- One `point` index per catalogue (partial): a search near a place reads only the catalogues that
-- are on, nearest first in each, never the points of one switched off but not dropped. A new
-- catalogue adds its own two.
CREATE INDEX catalogue_card_point_wikidata_idx ON catalogue_card USING gist (point) WHERE catalogue = 'wikidata';
CREATE INDEX catalogue_card_point_musicbrainz_idx ON catalogue_card USING gist (point) WHERE catalogue = 'musicbrainz';
CREATE INDEX catalogue_card_point_osm_idx ON catalogue_card USING gist (point) WHERE catalogue = 'osm';
CREATE INDEX catalogue_card_point_openalex_idx ON catalogue_card USING gist (point) WHERE catalogue = 'openalex';
CREATE INDEX catalogue_card_point_podcastindex_idx ON catalogue_card USING gist (point) WHERE catalogue = 'podcastindex';
CREATE INDEX catalogue_card_point_tranco_idx ON catalogue_card USING gist (point) WHERE catalogue = 'tranco';
CREATE INDEX catalogue_card_point_software_idx ON catalogue_card USING gist (point) WHERE catalogue = 'software';
CREATE INDEX catalogue_card_related_uris_idx ON catalogue_card USING gin (related_uris);
CREATE INDEX catalogue_card_type_fame_idx ON catalogue_card USING btree (catalogue, type, fame DESC, id);
CREATE INDEX catalogue_card_uri_idx ON catalogue_card USING btree (uri);

-- Each catalogue's import: what it does now (`phase`: idle, import (a full import, the first or
-- a re-import), update, failed, disabled), the run its `cursor` belongs to (`run`; NULL when none
-- is under way), so a restart resumes it, the generation of the last full import begun, where
-- the next update starts (`position`: a replication sequence, a time; kept from run to run), the
-- cards it holds, the upstream version read (a dump's date), when the last full import and the
-- last update finished, how current the cards are (`upstream_at`: the upstream time of the data
-- the last finished run left, which the update lag counts from), why an update asked for a full
-- import (`import_requested`, until that import begins as a new one), why it waits (`waiting`:
-- free disk, another large import), its errors, and the scope of its last finished full import
-- (`scope`: its `limit`, `region` or `min_citations`), the cards the full import of `generation`
-- has read so far (`generation_cards`; one of the same or a wider scope that read under half the
-- cards fails instead of removing the rest) and whether the operator let the next full import
-- remove what it did not read whatever its share (`shrink_allowed`, `pinakesd catalogue
-- allow-shrink`).
-- `pinakesd catalogue drop` deletes the row with the cards.
CREATE TABLE catalogue_state (
    catalogue text PRIMARY KEY CONSTRAINT catalogue_state_catalogue_check CHECK (is_catalogue(catalogue)),
    phase text NOT NULL DEFAULT 'idle' CONSTRAINT catalogue_state_phase_check CHECK (phase IN ('idle', 'import', 'update', 'failed', 'disabled')),
    run text CONSTRAINT catalogue_state_run_check CHECK (run IN ('import', 'update')),
    generation bigint NOT NULL DEFAULT 0 CONSTRAINT catalogue_state_generation_check CHECK (generation >= 0),
    cursor jsonb,
    position jsonb,
    cards bigint NOT NULL DEFAULT 0 CONSTRAINT catalogue_state_cards_check CHECK (cards >= 0),
    upstream_version text CONSTRAINT catalogue_state_upstream_version_check CHECK (octet_length(upstream_version) <= 256),
    last_import_at timestamp with time zone,
    last_update_at timestamp with time zone,
    upstream_at timestamp with time zone,
    -- These three are text for the operator (`pinakesd catalogue status`), each cut to 1 KiB by its
    -- writer.
    import_requested text CONSTRAINT catalogue_state_import_requested_check CHECK (octet_length(import_requested) <= 1024),
    waiting text CONSTRAINT catalogue_state_waiting_check CHECK (octet_length(waiting) <= 1024),
    last_error text CONSTRAINT catalogue_state_last_error_check CHECK (octet_length(last_error) <= 1024),
    error_count bigint NOT NULL DEFAULT 0 CONSTRAINT catalogue_state_error_count_check CHECK (error_count >= 0),
    updated_at timestamp with time zone NOT NULL DEFAULT now(),
    scope jsonb,
    generation_cards bigint NOT NULL DEFAULT 0 CONSTRAINT catalogue_state_generation_cards_check CHECK (generation_cards >= 0),
    shrink_allowed boolean NOT NULL DEFAULT false
);


-- The catalogue cards whose content changed in place, and those an update run inserted or
-- removed, for the text index (`search_index`); after a catalogue's first full import, also the
-- cards a full import inserts and removes. The first full import's cards are not queued: the index
-- reads the catalogue's cards again once that import has finished
-- (`catalogue_state.last_import_at`), so a first import of millions of cards never passes
-- through this table.
CREATE TABLE search_index_catalogue_queue (
    seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    card_id bigint NOT NULL
);

-- The inserted or removed cards of a statement: in an update run, and in a full import after the
-- catalogue's first.
CREATE FUNCTION queue_search_index_catalogue() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO search_index_catalogue_queue(card_id)
            SELECT n.id FROM changed_new n JOIN catalogue_state s ON s.catalogue = n.catalogue
            WHERE s.run = 'update' OR (s.run = 'import' AND s.last_import_at IS NOT NULL);
    ELSE
        INSERT INTO search_index_catalogue_queue(card_id)
            SELECT o.id FROM changed_old o JOIN catalogue_state s ON s.catalogue = o.catalogue
            WHERE s.run = 'update' OR (s.run = 'import' AND s.last_import_at IS NOT NULL);
    END IF;
    RETURN NULL;
END $$;
-- One card whose content changed, whatever wrote it (fired only then, so a full import's stamps
-- queue nothing and copy no rows).
CREATE FUNCTION queue_search_index_catalogue_card() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    INSERT INTO search_index_catalogue_queue(card_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
-- The search revision moves with a statement that inserted or removed cards (one that wrote none
-- moves nothing).
CREATE FUNCTION bump_search_revision_for_cards() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    IF current_setting('pinakes.search_write', true) IS DISTINCT FROM 'on' AND EXISTS (SELECT 1 FROM changed_cards) THEN
        INSERT INTO search_write DEFAULT VALUES;
        PERFORM set_config('pinakes.search_write', 'on', true);
    END IF;
    RETURN NULL;
END $$;

-- The host a catalogue card is listed under (`host=`): its link's, or for a root that is no web
-- link (`doi:…`) its identifier page's (`catalogue::card_host` answers the same; a test holds
-- them equal for every identifier page).
CREATE FUNCTION catalogue_card_host(uri text) RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT coalesce(substring(uri FROM '^[a-z][a-z0-9+.-]*://(\[[^]]*\]|[^/?#:]+)'), CASE lower(split_part(uri, ':', 1)) WHEN 'artic' THEN 'www.artic.edu' WHEN 'btc' THEN 'mempool.space' WHEN 'btc-signet' THEN 'mempool.space' WHEN 'btc-testnet' THEN 'mempool.space' WHEN 'btc-testnet4' THEN 'mempool.space' WHEN 'cleveland' THEN 'www.clevelandart.org' WHEN 'commons' THEN 'commons.wikimedia.org' WHEN 'docker' THEN 'hub.docker.com' WHEN 'doi' THEN 'doi.org' WHEN 'figshare' THEN 'figshare.com' WHEN 'fr' THEN 'www.federalregister.gov' WHEN 'gbif' THEN 'www.gbif.org' WHEN 'hf' THEN 'huggingface.co' WHEN 'ia' THEN 'archive.org' WHEN 'inat' THEN 'www.inaturalist.org' WHEN 'isbn' THEN 'openlibrary.org' WHEN 'isrc' THEN 'musicbrainz.org' WHEN 'lei' THEN 'www.gleif.org' WHEN 'ln' THEN 'mempool.space' WHEN 'ln-testnet' THEN 'mempool.space' WHEN 'mb' THEN 'musicbrainz.org' WHEN 'met' THEN 'www.metmuseum.org' WHEN 'nct' THEN 'clinicaltrials.gov' WHEN 'osm' THEN 'www.openstreetmap.org' WHEN 'pdb' THEN 'www.rcsb.org' WHEN 'pubchem' THEN 'pubchem.ncbi.nlm.nih.gov' WHEN 'ror' THEN 'ror.org' WHEN 'uniprot' THEN 'www.uniprot.org' WHEN 'usgs' THEN 'earthquake.usgs.gov' WHEN 'wikidata' THEN 'www.wikidata.org' WHEN 'zenodo' THEN 'zenodo.org' END) $$;
-- Searches without text read the cards near a point and on a host through these.
CREATE INDEX catalogue_card_geography_wikidata_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'wikidata';
CREATE INDEX catalogue_card_geography_musicbrainz_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'musicbrainz';
CREATE INDEX catalogue_card_geography_osm_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'osm';
CREATE INDEX catalogue_card_geography_openalex_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'openalex';
CREATE INDEX catalogue_card_geography_podcastindex_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'podcastindex';
CREATE INDEX catalogue_card_geography_tranco_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'tranco';
CREATE INDEX catalogue_card_geography_software_idx ON catalogue_card USING gist ((point::geography)) WHERE catalogue = 'software';
CREATE INDEX catalogue_card_host_idx ON catalogue_card USING btree (catalogue, catalogue_card_host(uri), fame DESC, id);
-- Catalogue cards with a point, counted per map cell and type at five levels (cells of
-- 360/2^(level+2) degrees: zooms 4, 6, 8, 10 and 12 of `GET /v1/map/clusters`), with the sums of their
-- coordinates: a map view at zoom 12 or less adds up the cells of the coarsest level that fits
-- its own cells instead of reading each card. Kept by the card table's triggers: a statement
-- whose cards have points takes one transaction-wide lock first, so two importers' transactions
-- touch the cells one after the other and never deadlock on them (and each touches them in key
-- order).
CREATE TABLE catalogue_map_cell (
    level smallint NOT NULL CONSTRAINT catalogue_map_cell_level_check CHECK (level IN (4, 6, 8, 10, 12)),
    cx integer NOT NULL,
    cy integer NOT NULL,
    catalogue text NOT NULL CONSTRAINT catalogue_map_cell_catalogue_check CHECK (is_catalogue(catalogue)),
    type text NOT NULL,
    cards bigint NOT NULL,
    sum_lon double precision NOT NULL,
    sum_lat double precision NOT NULL,
    PRIMARY KEY (level, cx, cy, catalogue, type)
);

-- The cells' counts follow the cards: a statement's removed and moved cards leave their old cells,
-- its new and moved cards join theirs. A point is compared as bytes (a dump restores the trigger
-- under an empty search path, where `geometry = geometry` does not resolve).
CREATE FUNCTION count_catalogue_cells() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    IF TG_OP = 'INSERT' THEN
        IF EXISTS (SELECT 1 FROM new_cards WHERE point IS NOT NULL) THEN
            PERFORM pg_advisory_xact_lock(1415, 1);
        END IF;
        INSERT INTO catalogue_map_cell AS m (level, cx, cy, catalogue, type, cards, sum_lon, sum_lat)
            SELECT l.level, floor((public.ST_X(n.point) + 180) / (360.0 / (4 << l.level)))::integer, floor((public.ST_Y(n.point) + 90) / (360.0 / (4 << l.level)))::integer,
                n.catalogue, n.type, count(*), sum(public.ST_X(n.point)), sum(public.ST_Y(n.point))
            FROM new_cards n CROSS JOIN (VALUES (4), (6), (8), (10), (12)) l(level)
            WHERE n.point IS NOT NULL
            GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 3, 4, 5
        ON CONFLICT (level, cx, cy, catalogue, type) DO UPDATE SET cards = m.cards + excluded.cards, sum_lon = m.sum_lon + excluded.sum_lon, sum_lat = m.sum_lat + excluded.sum_lat;
    ELSIF TG_OP = 'DELETE' THEN
        IF EXISTS (SELECT 1 FROM old_cards WHERE point IS NOT NULL) THEN
            PERFORM pg_advisory_xact_lock(1415, 1);
        END IF;
        INSERT INTO catalogue_map_cell AS m (level, cx, cy, catalogue, type, cards, sum_lon, sum_lat)
            SELECT l.level, floor((public.ST_X(o.point) + 180) / (360.0 / (4 << l.level)))::integer, floor((public.ST_Y(o.point) + 90) / (360.0 / (4 << l.level)))::integer,
                o.catalogue, o.type, -count(*), -sum(public.ST_X(o.point)), -sum(public.ST_Y(o.point))
            FROM old_cards o CROSS JOIN (VALUES (4), (6), (8), (10), (12)) l(level)
            WHERE o.point IS NOT NULL
            GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 3, 4, 5
        ON CONFLICT (level, cx, cy, catalogue, type) DO UPDATE SET cards = m.cards + excluded.cards, sum_lon = m.sum_lon + excluded.sum_lon, sum_lat = m.sum_lat + excluded.sum_lat;
    ELSE
        IF EXISTS (SELECT 1 FROM old_cards WHERE point IS NOT NULL) OR EXISTS (SELECT 1 FROM new_cards WHERE point IS NOT NULL) THEN
            PERFORM pg_advisory_xact_lock(1415, 1);
        END IF;
        INSERT INTO catalogue_map_cell AS m (level, cx, cy, catalogue, type, cards, sum_lon, sum_lat)
            SELECT l.level, floor((public.ST_X(o.point) + 180) / (360.0 / (4 << l.level)))::integer, floor((public.ST_Y(o.point) + 90) / (360.0 / (4 << l.level)))::integer,
                o.catalogue, o.type, -count(*), -sum(public.ST_X(o.point)), -sum(public.ST_Y(o.point))
            FROM old_cards o JOIN new_cards n ON n.id = o.id CROSS JOIN (VALUES (4), (6), (8), (10), (12)) l(level)
            WHERE o.point IS NOT NULL AND (public.ST_AsBinary(n.point) IS DISTINCT FROM public.ST_AsBinary(o.point) OR n.type <> o.type OR n.catalogue <> o.catalogue)
            GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 3, 4, 5
        ON CONFLICT (level, cx, cy, catalogue, type) DO UPDATE SET cards = m.cards + excluded.cards, sum_lon = m.sum_lon + excluded.sum_lon, sum_lat = m.sum_lat + excluded.sum_lat;
        INSERT INTO catalogue_map_cell AS m (level, cx, cy, catalogue, type, cards, sum_lon, sum_lat)
            SELECT l.level, floor((public.ST_X(n.point) + 180) / (360.0 / (4 << l.level)))::integer, floor((public.ST_Y(n.point) + 90) / (360.0 / (4 << l.level)))::integer,
                n.catalogue, n.type, count(*), sum(public.ST_X(n.point)), sum(public.ST_Y(n.point))
            FROM new_cards n JOIN old_cards o ON o.id = n.id CROSS JOIN (VALUES (4), (6), (8), (10), (12)) l(level)
            WHERE n.point IS NOT NULL AND (public.ST_AsBinary(n.point) IS DISTINCT FROM public.ST_AsBinary(o.point) OR n.type <> o.type OR n.catalogue <> o.catalogue)
            GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 3, 4, 5
        ON CONFLICT (level, cx, cy, catalogue, type) DO UPDATE SET cards = m.cards + excluded.cards, sum_lon = m.sum_lon + excluded.sum_lon, sum_lat = m.sum_lat + excluded.sum_lat;
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER catalogue_cells_inserted AFTER INSERT ON catalogue_card REFERENCING NEW TABLE AS new_cards FOR EACH STATEMENT EXECUTE FUNCTION count_catalogue_cells();
CREATE TRIGGER catalogue_cells_updated AFTER UPDATE ON catalogue_card REFERENCING OLD TABLE AS old_cards NEW TABLE AS new_cards FOR EACH STATEMENT EXECUTE FUNCTION count_catalogue_cells();
CREATE TRIGGER catalogue_cells_deleted AFTER DELETE ON catalogue_card REFERENCING OLD TABLE AS old_cards FOR EACH STATEMENT EXECUTE FUNCTION count_catalogue_cells();
CREATE FUNCTION clear_catalogue_cells() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
    DELETE FROM catalogue_map_cell;
    RETURN NULL;
END $$;
CREATE TRIGGER catalogue_cells_truncated AFTER TRUNCATE ON catalogue_card FOR EACH STATEMENT EXECUTE FUNCTION clear_catalogue_cells();

-- An OSM card's Wikidata item (its `wikidata` tag), for a deleted object's successor.
CREATE INDEX catalogue_card_osm_wikidata_idx ON catalogue_card USING btree ((details #>> '{tags,wikidata}')) WHERE catalogue = 'osm' AND details #>> '{tags,wikidata}' IS NOT NULL;
CREATE TRIGGER catalogue_card_search_index_insert AFTER INSERT ON catalogue_card REFERENCING NEW TABLE AS changed_new FOR EACH STATEMENT EXECUTE FUNCTION queue_search_index_catalogue();
CREATE TRIGGER catalogue_card_search_index_update AFTER UPDATE ON catalogue_card FOR EACH ROW WHEN (OLD.content_hash IS DISTINCT FROM NEW.content_hash OR OLD.catalogue IS DISTINCT FROM NEW.catalogue) EXECUTE FUNCTION queue_search_index_catalogue_card();
CREATE TRIGGER catalogue_card_search_index_delete AFTER DELETE ON catalogue_card REFERENCING OLD TABLE AS changed_old FOR EACH STATEMENT EXECUTE FUNCTION queue_search_index_catalogue();
-- Search pages without text read the cards: their ETag moves when a card is inserted, removed, or
-- changes what a page shows or how cards merge there (its content, keys and point; the point
-- compared as bytes, so a dump restores the trigger), never with a stamp or a write that changed
-- nothing.
CREATE TRIGGER catalogue_card_revision_insert AFTER INSERT ON catalogue_card REFERENCING NEW TABLE AS changed_cards FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision_for_cards();
CREATE TRIGGER catalogue_card_revision_delete AFTER DELETE ON catalogue_card REFERENCING OLD TABLE AS changed_cards FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision_for_cards();
CREATE TRIGGER catalogue_card_revision_update AFTER UPDATE ON catalogue_card FOR EACH ROW
    WHEN ((OLD.content_hash, OLD.catalogue, OLD.source, OLD.uri, OLD.type, OLD.name, OLD.other_names, OLD.description, OLD.labels, OLD.keys, OLD.fame) IS DISTINCT FROM (NEW.content_hash, NEW.catalogue, NEW.source, NEW.uri, NEW.type, NEW.name, NEW.other_names, NEW.description, NEW.labels, NEW.keys, NEW.fame)
        OR OLD.point::bytea IS DISTINCT FROM NEW.point::bytea)
    EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER catalogue_card_revision_truncate BEFORE TRUNCATE ON catalogue_card FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

-- A resource built from a catalogue card stays final and out of the fetch queue whatever writes
-- its row (a new reference, a requeue, `reextract`): it changes when its card does
-- (`catalogue_join_due`). Clearing `catalogue_card` gives the row back to the queue.
CREATE FUNCTION keep_catalogue_backed() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN
 NEW.final=true; NEW.dead_letter=false; NEW.next_attempt_at='infinity'; NEW.refresh_at=NULL; NEW.priority_at=NULL;
 RETURN NEW;
END $$;
CREATE TRIGGER keep_catalogue_backed BEFORE UPDATE ON resource FOR EACH ROW WHEN (NEW.catalogue_card IS NOT NULL) EXECUTE FUNCTION keep_catalogue_backed();

-- Resources whose join with the catalogues is to be checked again (`catalogue::join::drain`): one
-- built from a card that changed or went, one whose link a new or changed card holds (its `uri`,
-- a `url:` key, the canonical link of a key, a related link), and one a build was cut off in (its
-- mark until the build ends). The check builds the card again, joins another card, or gives the
-- link back to the fetch queue. (Links built from a catalogue switched off are given back by the
-- start's sweep, `catalogue::join::sweep`, without this queue.)
CREATE TABLE catalogue_join_due (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    due_at timestamp with time zone NOT NULL DEFAULT now(),
    -- When it was last marked: a check deletes the row only if it was not marked again meanwhile.
    marked_at timestamp with time zone NOT NULL DEFAULT clock_timestamp(),
    attempts integer NOT NULL DEFAULT 0 CONSTRAINT catalogue_join_due_attempts_check CHECK (attempts >= 0)
);
CREATE INDEX catalogue_join_due_due_idx ON catalogue_join_due USING btree (due_at, resource_id);

-- The extras of a resource built from a catalogue card, read in the background
-- (`catalogue::extras`): its article's Wikipedia summary. `due_at` while a read is due (set when
-- the card is built from a card or an article it has no read of, and when it is opened after
-- `extras::KEEP`), `read_at` of the last read.
CREATE TABLE catalogue_extras (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    due_at timestamp with time zone,
    read_at timestamp with time zone,
    -- The article the last read asked for: a card built again from the same one reads nothing.
    article text,
    attempts integer NOT NULL DEFAULT 0 CONSTRAINT catalogue_extras_attempts_check CHECK (attempts >= 0)
);
CREATE INDEX catalogue_extras_due_idx ON catalogue_extras USING btree (due_at, resource_id) WHERE due_at IS NOT NULL;

-- The canonical links a join key names (`catalogue::join::link_keys` reads each back to the key):
-- a row with one of these links joins a card holding the key when the card is written. A crate's
-- key spells `_` as `-` (crates.io takes them for one name): its pages with each, so a link to a
-- crate published with `_` names it too.
CREATE FUNCTION catalogue_key_links(key text) RETURNS text[]
    LANGUAGE sql IMMUTABLE
    AS $$
 SELECT CASE split_part(key,':',1)
  WHEN 'mb' THEN CASE WHEN split_part(key,':',2) IN ('artist','recording','release','release-group') THEN ARRAY['https://musicbrainz.org/'||split_part(key,':',2)||'/'||split_part(key,':',3)] END
  WHEN 'isrc' THEN ARRAY['https://musicbrainz.org/isrc/'||substr(key,6)]
  WHEN 'wikidata' THEN ARRAY['https://www.wikidata.org/wiki/'||substr(key,10)]
  WHEN 'doi' THEN ARRAY[key]
  WHEN 'arxiv' THEN ARRAY['https://arxiv.org/abs/'||substr(key,7),'https://arxiv.org/pdf/'||substr(key,7)]
  WHEN 'openalex' THEN ARRAY['https://openalex.org/'||substr(key,10)]
  WHEN 'crate' THEN ARRAY(SELECT p.page||n.name FROM unnest(ARRAY['https://crates.io/crates/','https://docs.rs/','https://docs.rs/crate/','https://lib.rs/crates/']) WITH ORDINALITY AS p(page,i)
   CROSS JOIN unnest(ARRAY[substr(key,7)]||CASE WHEN strpos(key,'-')>0 THEN ARRAY[replace(substr(key,7),'-','_')] ELSE '{}'::text[] END) WITH ORDINALITY AS n(name,j) ORDER BY p.i,n.j)
  WHEN 'android' THEN ARRAY['https://f-droid.org/packages/'||substr(key,9)||'/','https://f-droid.org/packages/'||substr(key,9),'https://f-droid.org/en/packages/'||substr(key,9)||'/']
  WHEN 'flathub' THEN ARRAY['https://flathub.org/apps/'||substr(key,9),'https://flathub.org/apps/details/'||substr(key,9)]
  WHEN 'homepage' THEN ARRAY['https://'||substr(key,10)||'/','https://www.'||substr(key,10)||'/','http://'||substr(key,10)||'/','http://www.'||substr(key,10)||'/']
 END
$$;
-- The resources new or changed cards name: the row of each card's link, of each `url:` key, of
-- the canonical links its keys name, and of each related link; and the rows built from a changed
-- or removed card. Each statement marks its rows in ID order, so two importers rarely wait on each
-- other. The generic plan is kept (a plan made for each row would cost more than the work).
CREATE FUNCTION catalogue_cards_inserted() RETURNS trigger
    LANGUAGE plpgsql
    SET plan_cache_mode TO 'force_generic_plan'
    AS $$ BEGIN
 INSERT INTO catalogue_join_due(resource_id)
  SELECT id FROM (
   SELECT r.id FROM new_cards n JOIN resource r ON r.uri=n.uri
   UNION SELECT r.id FROM new_cards n CROSS JOIN LATERAL unnest(n.keys) k JOIN resource r ON k LIKE 'url:%' AND r.id=substr(k,5)
   UNION SELECT r.id FROM new_cards n CROSS JOIN LATERAL unnest(n.keys) k CROSS JOIN LATERAL unnest(catalogue_key_links(k)) l JOIN resource r ON r.uri=l
   UNION SELECT r.id FROM new_cards n CROSS JOIN LATERAL unnest(n.related_uris) u JOIN resource r ON r.uri=u) x
  ORDER BY id
  ON CONFLICT (resource_id) DO UPDATE SET marked_at=clock_timestamp(),due_at=least(catalogue_join_due.due_at,now()),attempts=0;
 RETURN NULL;
END $$;
-- One card whose content hash changed (an import's stamp of an unchanged card fires nothing).
CREATE FUNCTION catalogue_card_changed() RETURNS trigger
    LANGUAGE plpgsql
    SET plan_cache_mode TO 'force_generic_plan'
    AS $$ BEGIN
 INSERT INTO catalogue_join_due(resource_id)
  SELECT id FROM (
   SELECT r.id FROM resource r WHERE r.catalogue_card=NEW.id
   UNION SELECT r.id FROM resource r WHERE r.uri IN (NEW.uri,OLD.uri)
   UNION SELECT r.id FROM unnest(NEW.keys||OLD.keys) k JOIN resource r ON k LIKE 'url:%' AND r.id=substr(k,5)
   UNION SELECT r.id FROM unnest(NEW.keys) k CROSS JOIN LATERAL unnest(catalogue_key_links(k)) l JOIN resource r ON r.uri=l
   UNION SELECT r.id FROM unnest(NEW.related_uris||OLD.related_uris) u JOIN resource r ON r.uri=u) x
  ORDER BY id
  ON CONFLICT (resource_id) DO UPDATE SET marked_at=clock_timestamp(),due_at=least(catalogue_join_due.due_at,now()),attempts=0;
 RETURN NULL;
END $$;
CREATE FUNCTION catalogue_cards_deleted() RETURNS trigger
    LANGUAGE plpgsql
    SET plan_cache_mode TO 'force_generic_plan'
    AS $$ BEGIN
 INSERT INTO catalogue_join_due(resource_id)
  SELECT id FROM (
   SELECT r.id FROM old_cards o JOIN resource r ON r.catalogue_card=o.id
   UNION SELECT r.id FROM old_cards o CROSS JOIN LATERAL unnest(o.related_uris) u JOIN resource r ON r.uri=u) x
  ORDER BY id
  ON CONFLICT (resource_id) DO UPDATE SET marked_at=clock_timestamp(),due_at=least(catalogue_join_due.due_at,now()),attempts=0;
 RETURN NULL;
END $$;
-- Every row built from a card or related to one is checked again.
CREATE FUNCTION catalogue_cards_truncated() RETURNS trigger
    LANGUAGE plpgsql
    SET plan_cache_mode TO 'force_generic_plan'
    AS $$ BEGIN
 INSERT INTO catalogue_join_due(resource_id)
  SELECT id FROM (
   SELECT id FROM resource WHERE catalogue_card IS NOT NULL
   UNION SELECT resource_id FROM resource_key WHERE source='catalogue') x
  ORDER BY id
  ON CONFLICT (resource_id) DO UPDATE SET marked_at=clock_timestamp(),due_at=least(catalogue_join_due.due_at,now()),attempts=0;
 RETURN NULL;
END $$;
CREATE TRIGGER catalogue_cards_inserted AFTER INSERT ON catalogue_card REFERENCING NEW TABLE AS new_cards FOR EACH STATEMENT EXECUTE FUNCTION catalogue_cards_inserted();
CREATE TRIGGER catalogue_card_changed AFTER UPDATE ON catalogue_card FOR EACH ROW WHEN (OLD.content_hash IS DISTINCT FROM NEW.content_hash) EXECUTE FUNCTION catalogue_card_changed();
CREATE TRIGGER catalogue_cards_deleted AFTER DELETE ON catalogue_card REFERENCING OLD TABLE AS old_cards FOR EACH STATEMENT EXECUTE FUNCTION catalogue_cards_deleted();
CREATE TRIGGER catalogue_cards_truncated AFTER TRUNCATE ON catalogue_card FOR EACH STATEMENT EXECUTE FUNCTION catalogue_cards_truncated();


-- ── Rows the code updates in place ───────────────────────────────────────────────────────────

-- The search revision starts at 1.
INSERT INTO search_write DEFAULT VALUES;
INSERT INTO group_runtime DEFAULT VALUES;
