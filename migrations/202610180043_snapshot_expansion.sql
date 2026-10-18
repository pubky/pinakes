-- The calendar expansion (extractor version and event.utc_series_wall_clock mode) that produced
-- each applied snapshot. Hour-shifted occurrence keys pair as moves only when it changes, i.e.
-- when Pinakes itself re-expanded the feed; NULL (snapshots before this column) counts as a change.
ALTER TABLE event_fetch ADD COLUMN expansion text;
