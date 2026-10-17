-- The extractor version that produced the stored document (resolver::EXTRACTOR_VERSIONS), so
-- parser changes reach documents fetched before them: at startup the daemon re-fetches ok rows
-- below their resolver's current version (queue.reextract_on_upgrade). Every resolved row is
-- marked 0 here, so the first start after this release re-extracts everything once, spread over
-- queue.reextract_window_secs and throttled by the normal per-host limits.
ALTER TABLE resource ADD COLUMN extracted_with integer;
UPDATE resource SET extracted_with=0 WHERE resolved_at IS NOT NULL;
