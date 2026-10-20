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
