-- Derived links keep the instant their event asked for (`until`, before the horizon cap), so
-- their expiry can follow the horizon as it moves (round 24, E1), and the time each parent first
-- listed each child (`first_seen_at`), so a NIP-52 calendar measures its own wait for a member
-- (E2). Both columns are added without rewriting or updating rows: existing links have no
-- `until` and keep their expiry, and count as first seen at the upgrade.
ALTER TABLE resource_derivation
  ADD COLUMN until timestamptz,
  ADD COLUMN first_seen_at timestamptz NOT NULL DEFAULT now();
