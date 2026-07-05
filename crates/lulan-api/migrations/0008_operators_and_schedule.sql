-- Trip identity & schedule (search enrichment).
--
-- Cross-modal identity: an OPERATOR (carrier/agency) runs a scheduled
-- service with a passenger-facing SERVICE NUMBER (the "flight number"
-- equivalent), on a physical VEHICLE (already modelled as `resources`).
--
-- Schedule: per-stop arrival/departure offsets (minutes from the trip's
-- origin departure) let search report real departure AND arrival times —
-- and duration — for any journey span. Offsets live on the route pattern,
-- so every trip on a route shares them; the trip's absolute times are
-- `departs_at + offset`. Nullable/zero-defaulted so existing data stays
-- valid until the seeder (or a GTFS import) fills them in.

CREATE TABLE operators (
    id   uuid PRIMARY KEY,
    code text NOT NULL UNIQUE,
    name text NOT NULL
);

ALTER TABLE trips ADD COLUMN operator_id uuid REFERENCES operators (id);
-- Passenger-facing service designator: flight/train/service number, e.g.
-- "5J 557", "LUL 501", "ICE 573".
ALTER TABLE trips ADD COLUMN service_number text;

-- Minutes from the trip's origin departure. Origin stop = (0, 0); the
-- final stop's depart offset equals its arrive offset. arrive ≤ depart at
-- every stop (dwell time).
ALTER TABLE route_stops ADD COLUMN arrive_offset_min integer NOT NULL DEFAULT 0;
ALTER TABLE route_stops ADD COLUMN depart_offset_min integer NOT NULL DEFAULT 0;
