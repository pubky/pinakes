ALTER TABLE group_dirty ADD COLUMN fence_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE group_input ADD COLUMN last_error text;
CREATE TABLE group_runtime(id boolean PRIMARY KEY DEFAULT true CHECK(id),settings jsonb,pair_budget_total bigint NOT NULL DEFAULT 0);
INSERT INTO group_runtime DEFAULT VALUES;
