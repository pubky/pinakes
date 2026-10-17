-- The worker kind that extracted a document (web runs event, event_platform and disabled card
-- kinds too). NULL for documents saved before this column: they compare against their route.
ALTER TABLE resource ADD COLUMN extracted_by text;
-- Points (geo: and map links) no longer claim their own URL as a group key; regroup them.
INSERT INTO group_dirty(resource_id) SELECT id FROM resource WHERE doc->>'source'='geo' ON CONFLICT DO NOTHING;
