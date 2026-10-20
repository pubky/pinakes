-- The search revision without a shared row lock. Each transaction that changes searchable rows
-- adds one row to search_write (a sequence value: no row is shared between writers), and a
-- reader's revision is the number of write transactions its snapshot sees. A later snapshot sees
-- every transaction an earlier one saw, so an equal revision means the same writes, and a write
-- still in flight is invisible together with its data.

CREATE SEQUENCE search_write_id_seq;

CREATE TABLE search_write (
    id bigint NOT NULL DEFAULT nextval('search_write_id_seq') PRIMARY KEY,
    -- How many write transactions the row counts (more than one after `compact_search_writes`).
    writes bigint NOT NULL DEFAULT 1
);
ALTER SEQUENCE search_write_id_seq OWNED BY search_write.id;

-- The revision continues from the old counter.
INSERT INTO search_write(writes) SELECT revision FROM search_revision WHERE id=1;

CREATE OR REPLACE FUNCTION bump_search_revision() RETURNS trigger
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

DROP TABLE search_revision;

CREATE VIEW search_revision AS
    SELECT 1 AS id, coalesce(sum(writes), 0)::bigint AS revision FROM search_write;

-- A writer of a resource's child rows locks the resource's row before marking it dirty, as a save
-- does (its row, then the mark) and as an inserted child's foreign key does. A removed child takes
-- no lock on the resource before this trigger runs, and the revision trigger that follows updates
-- the resource's row: without this, a removal and a save of the same resource could deadlock.
CREATE OR REPLACE FUNCTION mark_group_dirty() RETURNS trigger
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
