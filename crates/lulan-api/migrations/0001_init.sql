-- Baseline migration: proves the migration pipeline end-to-end.
-- Domain tables (routes, trips, capacity units, occupancy) arrive in Phase 1.
CREATE TABLE lulan_meta (
    key        text PRIMARY KEY,
    value      text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO lulan_meta (key, value) VALUES ('schema_baseline', '0001');
