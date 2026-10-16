-- Keep bounded timeout escalation across worker restarts without changing accepted facts.
ALTER TABLE resource ADD COLUMN timeout_streak integer NOT NULL DEFAULT 0 CHECK(timeout_streak>=0);
