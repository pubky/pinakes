-- Settings the daemon applied to stored data, so a configuration change can be detected at
-- startup (event.utc_series_wall_clock: calendars re-parse once when it changes).
CREATE TABLE instance_setting(key text PRIMARY KEY, value text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now());
