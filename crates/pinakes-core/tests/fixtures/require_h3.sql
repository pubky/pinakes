-- Run in every test database after the schema: the check pinakesd makes at startup
-- (`pinakes_core::check_extensions`). Clustering calls h3_latlng_to_cell(point, integer), which
-- h3-pg added in 4.2.3. Without it every database test stops here with the version found, instead
-- of a `cluster=` search answering 500. `CREATE EXTENSION IF NOT EXISTS h3` never upgrades, so a
-- template1 that holds an older h3 gives every test database that version.
DO $$
BEGIN
    IF to_regprocedure('h3_latlng_to_cell(point,integer)') IS NULL THEN
        RAISE EXCEPTION 'h3 4.2.3 or newer required, found %',
            coalesce((SELECT extversion FROM pg_extension WHERE extname = 'h3'), 'none')
            USING HINT = 'Upgrade the h3 package, then run ALTER EXTENSION h3 UPDATE in each database that has it (template1 included).';
    END IF;
END $$;
