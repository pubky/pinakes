CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE resource (
    id text PRIMARY KEY CHECK (id ~ '^[0-9a-f]{32}$'),
    uri text NOT NULL UNIQUE,
    scheme text NOT NULL,
    host text,
    type text,
    type_path text[] NOT NULL DEFAULT '{}',
    name text,
    category text,
    point geometry(Point,4326),
    start_at timestamptz,
    end_at timestamptz,
    venue_id text,
    doc jsonb NOT NULL DEFAULT '{}',
    search_text tsvector GENERATED ALWAYS AS
      (to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(doc->>'description', ''))) STORED,
    resolver text,
    resolved_at timestamptz,
    refresh_at timestamptz,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('ok','pending','missing','unreachable','error')),
    created_at timestamptz NOT NULL DEFAULT now(),
    orphaned_at timestamptz DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz,
    generation bigint NOT NULL DEFAULT 0,
    dead_letter boolean NOT NULL DEFAULT false,
    last_error text,
    CHECK (octet_length(doc::text) <= 8192)
);
CREATE INDEX resource_point_idx ON resource USING gist(point);
CREATE INDEX resource_type_path_idx ON resource USING gin(type_path);
CREATE INDEX resource_search_idx ON resource USING gin(search_text);
CREATE INDEX resource_name_trgm_idx ON resource USING gin(name gin_trgm_ops);
CREATE INDEX resource_category_idx ON resource(category);
CREATE INDEX resource_start_idx ON resource(start_at);
CREATE INDEX resource_host_idx ON resource(host);
CREATE INDEX resource_venue_idx ON resource(venue_id);
CREATE INDEX resource_queue_idx ON resource(resolver, next_attempt_at) WHERE NOT dead_letter;

CREATE TABLE reference (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('tag','reply','embed','collection','event')),
    author text NOT NULL,
    label text,
    object_path text NOT NULL,
    recurrence_id text,
    start_at timestamptz,
    end_at timestamptz,
    created_at timestamptz NOT NULL,
    PRIMARY KEY(author, object_path)
);
CREATE INDEX reference_resource_idx ON reference(resource_id);
CREATE INDEX reference_label_idx ON reference(label, resource_id);
CREATE INDEX reference_author_idx ON reference(author);
CREATE INDEX reference_start_idx ON reference(start_at);

CREATE TABLE resource_score (
    resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE,
    score double precision NOT NULL,
    updated_at timestamptz NOT NULL,
    newest_at timestamptz NOT NULL
);

CREATE TABLE occurrence (
    resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
    uid text NOT NULL,
    recurrence_id text NOT NULL DEFAULT '',
    start_at timestamptz NOT NULL,
    end_at timestamptz,
    name text,
    venue_id text,
    status text NOT NULL,
    url text,
    event_resource_id text REFERENCES resource(id) ON DELETE SET NULL,
    PRIMARY KEY(resource_id, uid, recurrence_id)
);
CREATE INDEX occurrence_start_idx ON occurrence(start_at);
CREATE INDEX occurrence_venue_idx ON occurrence(venue_id);

CREATE TABLE feed_cursor (
    nexus text PRIMARY KEY,
    cursor bigint NOT NULL DEFAULT 0 CHECK (cursor >= 0),
    epoch text,
    feed_length bigint NOT NULL DEFAULT 0 CHECK (feed_length >= 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);
