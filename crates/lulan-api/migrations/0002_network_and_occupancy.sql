-- Transit network and segment occupancy (Phase 1).
-- Occupancy invariants live in the database so a double-sell is
-- unrepresentable regardless of application bugs (ADR 0002).

CREATE TABLE locations (
    id       uuid PRIMARY KEY,
    code     text NOT NULL UNIQUE,
    name     text NOT NULL,
    timezone text NOT NULL DEFAULT 'UTC'
);

CREATE TABLE routes (
    id   uuid PRIMARY KEY,
    code text NOT NULL UNIQUE,
    name text NOT NULL
);

CREATE TABLE route_stops (
    route_id    uuid NOT NULL REFERENCES routes (id) ON DELETE CASCADE,
    stop_index  smallint NOT NULL CHECK (stop_index >= 0),
    location_id uuid NOT NULL REFERENCES locations (id),
    PRIMARY KEY (route_id, stop_index),
    UNIQUE (route_id, location_id)
);

CREATE TABLE resources (
    id   uuid PRIMARY KEY,
    code text NOT NULL UNIQUE,
    name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('bus', 'ferry', 'aircraft', 'other'))
);

CREATE TABLE capacity_units (
    id            uuid PRIMARY KEY,
    resource_id   uuid NOT NULL REFERENCES resources (id) ON DELETE CASCADE,
    kind          text NOT NULL CHECK (kind IN ('seat', 'pool')),
    code          text NOT NULL,
    fare_class    text CHECK (kind <> 'seat' OR fare_class IS NOT NULL),
    pool_capacity integer CHECK ((kind = 'pool') = (pool_capacity IS NOT NULL)),
    UNIQUE (resource_id, code)
);

CREATE TABLE trips (
    id            uuid PRIMARY KEY,
    route_id      uuid NOT NULL REFERENCES routes (id),
    resource_id   uuid NOT NULL REFERENCES resources (id),
    service_date  date NOT NULL,
    departs_at    timestamptz NOT NULL,
    -- u64 occupancy masks cap trips at 64 segments (lulan_engine MAX_SEGMENTS)
    segment_count smallint NOT NULL CHECK (segment_count BETWEEN 1 AND 64),
    UNIQUE (route_id, resource_id, departs_at)
);

CREATE INDEX trips_by_date_route ON trips (service_date, route_id);

-- One row per (trip, seat). occupied_mask bit i = segment i is sold.
-- Stored as bigint; the Rust side reinterprets it as u64.
CREATE TABLE seat_occupancy (
    trip_id       uuid NOT NULL REFERENCES trips (id) ON DELETE CASCADE,
    unit_id       uuid NOT NULL REFERENCES capacity_units (id),
    occupied_mask bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (trip_id, unit_id)
);

-- One row per (trip, pool). remaining[i] = units left on segment i-1
-- (Postgres arrays are 1-based). The CHECK makes overselling a pool
-- impossible at the database level.
CREATE TABLE pool_occupancy (
    trip_id   uuid NOT NULL REFERENCES trips (id) ON DELETE CASCADE,
    unit_id   uuid NOT NULL REFERENCES capacity_units (id),
    remaining integer[] NOT NULL CHECK (0 <= ALL (remaining)),
    PRIMARY KEY (trip_id, unit_id)
);
