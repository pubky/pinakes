CREATE TABLE search_revision (id integer PRIMARY KEY CHECK(id=1), revision bigint NOT NULL);
INSERT INTO search_revision VALUES(1,1);
CREATE FUNCTION bump_search_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
    UPDATE search_revision SET revision=revision+1 WHERE id=1;
    RETURN NULL;
END $$;
-- BEFORE STATEMENT establishes one lock order before writers acquire resource/OSM row locks.
CREATE TRIGGER resource_revision_insert BEFORE INSERT OR DELETE ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER resource_revision_update BEFORE UPDATE OF uri,scheme,host,resolver,doc,status,name,type,type_path,category,point,start_at,end_at,osm_version,resolved_at,refresh_at ON resource FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER osm_revision_insert BEFORE INSERT OR DELETE ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER osm_revision_update BEFORE UPDATE OF doc,status,name,type,type_path,category,point,bbox ON osm_place FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER reference_revision BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON reference FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER score_revision BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON resource_score FOR EACH STATEMENT EXECUTE FUNCTION bump_search_revision();
