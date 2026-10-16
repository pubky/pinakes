-- Group computations fence only inputs they read. Unrelated ingestion remains concurrent.
ALTER TABLE resource ADD COLUMN group_revision bigint NOT NULL DEFAULT 0;
CREATE FUNCTION bump_group_resource_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 NEW.group_revision=OLD.group_revision+1;
 RETURN NEW;
END $$;
CREATE TRIGGER group_resource_revision BEFORE UPDATE OF doc,status,uri,scheme,host,point,start_at,end_at,type,name,osm_version ON resource
 FOR EACH ROW EXECUTE FUNCTION bump_group_resource_revision();
CREATE FUNCTION bump_group_child_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 UPDATE resource SET group_revision=group_revision+1
 WHERE id IN (CASE WHEN TG_OP<>'INSERT' THEN OLD.resource_id END, CASE WHEN TG_OP<>'DELETE' THEN NEW.resource_id END);
 RETURN NULL;
END $$;
CREATE TRIGGER group_occurrence_revision AFTER INSERT OR UPDATE OR DELETE ON occurrence FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
CREATE TRIGGER group_reference_revision AFTER INSERT OR UPDATE OR DELETE ON reference FOR EACH ROW EXECUTE FUNCTION bump_group_child_revision();
