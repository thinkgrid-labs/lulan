-- Passengers and ticketing (Phase 5).
-- One order = one itinerary with N passengers; tickets hang off passengers.

CREATE TABLE passengers (
    id             uuid PRIMARY KEY,
    order_id       uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    full_name      text NOT NULL,
    -- A fare input, not metadata: senior/PWD discounts are legally
    -- mandated in the PH market.
    passenger_type text NOT NULL CHECK (passenger_type IN
        ('adult', 'child', 'senior', 'pwd', 'infant')),
    -- Optional: lets operators verify age-based fares at boarding.
    birthdate      date,
    created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX passengers_by_order ON passengers (order_id);

-- Seat items belong to exactly one passenger; pool items (cargo, vehicle
-- slots) stay order-level (NULL).
ALTER TABLE order_items ADD COLUMN passenger_id uuid REFERENCES passengers (id);

-- Ed25519 ticket signing keys. `kid` travels inside every ticket so
-- validators can pick the right public key; rotation = insert new active
-- key, old tickets keep verifying against the old public key.
CREATE TABLE ticket_keys (
    kid        text PRIMARY KEY,
    secret     bytea NOT NULL,
    public     bytea NOT NULL,
    active     boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tickets (
    id           uuid PRIMARY KEY,
    order_id     uuid NOT NULL REFERENCES orders (id),
    passenger_id uuid NOT NULL REFERENCES passengers (id),
    trip_id      uuid NOT NULL REFERENCES trips (id),
    unit_id      uuid NOT NULL REFERENCES capacity_units (id),
    status       text NOT NULL DEFAULT 'issued'
        CHECK (status IN ('issued', 'boarded', 'void')),
    -- The full signed token (LT1.<payload>.<sig>); rendered as a QR
    -- client-side.
    token        text NOT NULL,
    kid          text NOT NULL REFERENCES ticket_keys (kid),
    issued_at    timestamptz NOT NULL DEFAULT now(),
    boarded_at   timestamptz,
    UNIQUE (order_id, passenger_id, unit_id)
);

CREATE INDEX tickets_by_order ON tickets (order_id);
CREATE INDEX tickets_by_trip ON tickets (trip_id);

-- Offline boarding journal: devices scan offline, then sync batches.
-- Idempotency key (ticket, device, scanned_at) makes replayed uploads
-- harmless; duplicate scans across devices are visible post-hoc — the
-- honest replay-detection model documented in the plan.
CREATE TABLE scan_events (
    id          bigserial PRIMARY KEY,
    ticket_id   uuid NOT NULL REFERENCES tickets (id),
    device_id   text NOT NULL,
    scanned_at  timestamptz NOT NULL,
    result      text NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (ticket_id, device_id, scanned_at)
);
