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

-- A changed key regroups its resource; changed links change the cards a search page shows.
CREATE TRIGGER resource_key_group_dirty AFTER INSERT OR DELETE OR UPDATE OF key ON resource_key FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER resource_key_group_revision AFTER INSERT OR DELETE OR UPDATE OF key ON resource_key FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
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

-- The group hint on a card: the group's ID and size. The members it names, with the derived
-- links, are the card's `also_on` list (below).
CREATE OR REPLACE FUNCTION group_hint(unit text) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
 SELECT jsonb_build_object('id',u.group_id,'size',(SELECT count(*) FROM group_unit x WHERE x.group_id=u.group_id))
 FROM group_unit u WHERE u.unit_id=unit AND u.group_id IS NOT NULL
$$;

-- A card's `also_on`: the other members of its group (tagged links, with their ID: other
-- resources first, one each by ID, then other units of its own resource), then the live derived
-- links of the card's resource and of every member, by service name, each URL once and never
-- the card itself; at most `cap` entries. `resource` is the card's resource ID for a resource's
-- card, and the unit for an occurrence or an event reference (whose feed's links are not its
-- own). `members` false (groups switched off) leaves the group out. Read at
-- request time: OSM-derived values never enter these tables.
-- A card (`per_site`) shows a derived link only for a service no tagged member is on, and one
-- per service (the one most members name); the details read (`per_site` false) lists them all.
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
   d.host,d.doc->>'site' AS site,d.unit_id=unit AS own,d.id=(SELECT resource_id FROM grp) AS mine
   FROM picked p
   CROSS JOIN LATERAL (SELECT * FROM group_member_document WHERE unit_id=p.unit_id OFFSET 0) d),
 tagged AS (SELECT id,uri,min(host) AS host,min(site) AS site,bool_or(mine) AS mine FROM m GROUP BY id,uri HAVING NOT bool_or(own)),
 owners AS (SELECT resource AS resource_id UNION SELECT u.resource_id FROM grp JOIN group_unit u USING(group_id)),
 derived AS (SELECT DISTINCT ON (l.link_id) l.link_id,l.uri,l.site,l.via,l.kind,l.at,
     count(*) OVER (PARTITION BY l.link_id) AS named
   FROM derived_link l JOIN owners o USING(resource_id)
   WHERE l.expires_at>now() AND NOT EXISTS(SELECT 1 FROM owners x WHERE x.resource_id=l.link_id)
     AND NOT EXISTS(SELECT 1 FROM tagged t WHERE t.id=l.link_id)
   ORDER BY l.link_id,l.seen_at DESC,l.via),
 entries AS (SELECT CASE WHEN mine THEN 1 ELSE 0 END AS o,id AS k1,uri AS k2,0::bigint AS rank,coalesce(lower(site),uri) AS service,jsonb_build_object('id',id,'uri',uri,'host',host,'site',site) AS e FROM tagged
   UNION ALL SELECT 2,lower(site),uri,-named,lower(site),jsonb_strip_nulls(jsonb_build_object('uri',uri,'site',site,'derived',true,'via',via,'kind',kind,'at',at)) FROM derived),
 shown AS (SELECT * FROM (SELECT *,row_number() OVER (PARTITION BY service ORDER BY o,rank,k2) AS nth FROM entries) x WHERE NOT per_site OR o<2 OR nth=1)
 SELECT coalesce((SELECT jsonb_agg(e ORDER BY o,k1,k2) FROM (SELECT * FROM shown ORDER BY o,k1,k2 LIMIT cap) x),'[]'::jsonb)
$$;

-- A music service's and Wikipedia's URL forms are spellings of one object (`music::parse`,
-- `wikipedia::parse`).
ALTER TABLE resource_alias DROP CONSTRAINT resource_alias_basis;
ALTER TABLE resource_alias ADD CONSTRAINT resource_alias_basis CHECK ((basis = ANY (ARRAY['cursor'::text, 'osm'::text, 'identifier'::text, 'service'::text, 'tracking'::text, 'canonical'::text])));

-- Joins the last grouping run refused because they would put two values of one single-object
-- hub key namespace (an MBID, a Q-id, a Discogs ID) in one group.
ALTER TABLE group_input ADD COLUMN key_conflicts integer DEFAULT 0 NOT NULL;

-- `sort=trending` counts the references made inside a window.
CREATE INDEX reference_created_idx ON reference USING btree (created_at);
