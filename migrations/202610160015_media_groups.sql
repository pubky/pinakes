-- Group membership is a view over immutable resource/occurrence identities. No upstream
-- OSM facts are copied here: names/images/points are hydrated from search_unit on reads.
CREATE TABLE "group" (
 id text PRIMARY KEY CHECK(id ~ '^[0-9a-f]{32}$'),
 kind text, name text CHECK(octet_length(name)<=512), image text CHECK(octet_length(image)<=2048),
 best_unit text, curated boolean NOT NULL DEFAULT false
);
CREATE TABLE group_unit (
 unit_id text PRIMARY KEY CHECK(unit_id ~ '^[0-9a-f]{32}$'),
 resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
 occurrence_key text NOT NULL DEFAULT '',
 group_id text REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX group_unit_resource_idx ON group_unit(resource_id);
CREATE INDEX group_unit_group_idx ON group_unit(group_id);
CREATE TABLE resource_group (
 unit_id text NOT NULL REFERENCES group_unit(unit_id) ON DELETE CASCADE,
 resource_id text NOT NULL REFERENCES resource(id) ON DELETE CASCADE,
 group_id text REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED,
 key text NOT NULL CHECK(octet_length(key)<=2200), basis text NOT NULL,
 confidence double precision NOT NULL CHECK(confidence BETWEEN 0 AND 1),
 PRIMARY KEY(unit_id,key)
);
CREATE INDEX resource_group_resource_idx ON resource_group(resource_id);
CREATE INDEX resource_group_key_idx ON resource_group(key);
CREATE INDEX resource_group_group_idx ON resource_group(group_id);
CREATE TABLE group_override (
 unit_id text PRIMARY KEY REFERENCES group_unit(unit_id) ON DELETE CASCADE,
 group_id text NOT NULL REFERENCES "group"(id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX group_override_group_idx ON group_override(group_id);
CREATE TABLE group_score (
 group_id text PRIMARY KEY REFERENCES "group"(id) ON DELETE CASCADE,
 score double precision NOT NULL, updated_at timestamptz NOT NULL,
 newest_at timestamptz, taggers bigint NOT NULL, references_count bigint NOT NULL
);
-- An upgrade queues existing resources for bounded, transactional grouping.
CREATE TABLE group_dirty(resource_id text PRIMARY KEY REFERENCES resource(id) ON DELETE CASCADE);
INSERT INTO group_dirty SELECT id FROM resource;
CREATE FUNCTION mark_group_dirty() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF TG_TABLE_NAME='resource' THEN
  INSERT INTO group_dirty VALUES(NEW.id) ON CONFLICT DO NOTHING;
 ELSE
  INSERT INTO group_dirty SELECT id FROM resource WHERE id=CASE WHEN TG_OP='DELETE' THEN OLD.resource_id ELSE NEW.resource_id END ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER group_resource_dirty AFTER INSERT OR UPDATE OF doc ON resource FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER group_occurrence_dirty AFTER INSERT OR UPDATE OR DELETE ON occurrence FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER group_reference_dirty AFTER INSERT OR UPDATE OR DELETE ON reference FOR EACH ROW EXECUTE FUNCTION mark_group_dirty();
CREATE TRIGGER groups_revision BEFORE INSERT OR UPDATE OR DELETE ON "group" FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER group_units_revision BEFORE INSERT OR UPDATE OR DELETE ON group_unit FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();

CREATE FUNCTION group_card(gid text) RETURNS jsonb LANGUAGE sql STABLE AS $$
 SELECT jsonb_build_object('id',g.id,'kind',coalesce(g.kind,b.type),'name',coalesce(g.name,b.name),
 'image',coalesce(g.image,b.doc->>'image'),'taggers',coalesce(s.taggers,0),'references',coalesce(s.references_count,0),'curated',g.curated,
 'members',coalesce((SELECT jsonb_agg(jsonb_build_object('resource_id',r.id,'uri',r.uri,'site',coalesce(r.doc->>'site',r.host,'')) ||
 CASE WHEN u.occurrence_key='' THEN '{}'::jsonb ELSE jsonb_build_object('occurrence',jsonb_build_object('uid',(u.occurrence_key::jsonb)->>0,'recurrence_id',nullif((u.occurrence_key::jsonb)->>1,''))) END ORDER BY u.unit_id)
 FROM group_unit u JOIN resource r ON r.id=u.resource_id WHERE u.group_id=g.id),'[]'::jsonb))
 FROM "group" g LEFT JOIN group_score s ON s.group_id=g.id LEFT JOIN search_unit b ON b.unit_id=g.best_unit WHERE g.id=gid
$$;
-- Deletion of the previous best member cannot hide the surviving group. Rebuilding
-- its evidence/scores is queued, and the read pointer is repaired in this transaction.
CREATE FUNCTION group_member_deleted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 INSERT INTO group_dirty SELECT resource_id FROM group_unit WHERE group_id=OLD.group_id ON CONFLICT DO NOTHING;
 UPDATE "group" SET best_unit=(SELECT u.unit_id FROM group_unit u JOIN search_unit s USING(unit_id) WHERE u.group_id=OLD.group_id ORDER BY (s.point IS NOT NULL) DESC,(s.status='ok') DESC,(s.start_at IS NOT NULL) DESC,u.unit_id LIMIT 1)
 WHERE id=OLD.group_id AND best_unit=OLD.unit_id;
 DELETE FROM "group" g WHERE id=OLD.group_id AND NOT curated AND NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.group_id=g.id);
 RETURN NULL;
END $$;
CREATE TRIGGER group_member_deleted AFTER DELETE ON group_unit FOR EACH ROW EXECUTE FUNCTION group_member_deleted();
-- Upgrade existing identifier routes and common media hosts without reviving failed
-- jobs. Other registry providers are classified on insertion; existing pages can be
-- promoted by discovered media metadata during their normal refresh.
UPDATE resource SET resolver=CASE WHEN scheme='isbn' THEN 'isbn' WHEN scheme='doi' OR host='doi.org' THEN 'doi' ELSE resolver END,
 next_attempt_at=greatest(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')) WHERE scheme IN ('isbn','doi') OR host='doi.org';

CREATE VIEW grouped_search_unit AS
 SELECT b.id,b.uri,b.scheme,b.host,coalesce(g.kind,b.type) AS type,b.type_path,coalesce(g.name,b.name) AS name,b.category,
 b.point,b.start_at,b.end_at,b.venue_id,b.doc || CASE WHEN g.name IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('name',g.name) END || CASE WHEN g.image IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('image',g.image) END AS doc,
 b.search_text,b.resolver,b.resolved_at,b.refresh_at,b.status,b.osm_type,b.osm_id,g.id AS unit_id,b.occurrence,b.event,b.event_author,group_card(g.id) AS "group"
 FROM "group" g JOIN search_unit b ON b.unit_id=g.best_unit
 UNION ALL
 SELECT b.*,NULL::jsonb FROM search_unit b WHERE NOT EXISTS(SELECT 1 FROM group_unit u WHERE u.unit_id=b.unit_id AND u.group_id IS NOT NULL);

UPDATE resource SET resolver='nostr',next_attempt_at=greatest(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')) WHERE scheme='nostr' AND uri ~ '^nostr:(npub|nprofile|note|nevent)1';
UPDATE resource SET resolver='media',next_attempt_at=greatest(next_attempt_at,now()+((('x'||substr(id,1,8))::bit(32)::bigint % 3600)*interval '1 second')) WHERE resolver='web' AND host IN ('youtube.com','www.youtube.com','youtu.be','vimeo.com','www.vimeo.com','open.spotify.com','soundcloud.com');
