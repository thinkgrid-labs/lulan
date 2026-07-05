-- Itineraries (Phase 6.5): an order is an itinerary, not a single
-- departure. trip_id moves from orders down to order_items — claims are
-- per-item and all trips share one Postgres, so cross-leg atomicity is
-- inherited from the existing transaction. Journey grouping is derived
-- (items sharing a trip), never stored.

ALTER TABLE order_items ADD COLUMN trip_id uuid REFERENCES trips (id);

UPDATE order_items oi
SET trip_id = o.trip_id
FROM orders o
WHERE o.id = oi.order_id;

ALTER TABLE order_items ALTER COLUMN trip_id SET NOT NULL;

CREATE INDEX order_items_by_trip ON order_items (trip_id);

-- One vessel serves both directions, so a round trip books the SAME
-- capacity unit on two trips — identity keys must include the trip.
ALTER TABLE order_items DROP CONSTRAINT order_items_pkey;
ALTER TABLE order_items ADD PRIMARY KEY (order_id, trip_id, unit_id);
ALTER TABLE tickets DROP CONSTRAINT tickets_order_id_passenger_id_unit_id_key;
ALTER TABLE tickets ADD UNIQUE (order_id, trip_id, passenger_id, unit_id);

ALTER TABLE orders DROP COLUMN trip_id;
