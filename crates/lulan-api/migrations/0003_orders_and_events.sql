-- Orders, the append-only event log, and the transactional outbox (Phase 3).
-- ADR 0001: events live in Postgres; external streaming is an optional sink.

-- Append-only event log. UPDATE/DELETE are blocked by trigger because
-- immutable event history is a security property (PRD §10), not a
-- convention. (stream_id, stream_seq) gives per-stream ordering and
-- optimistic concurrency.
CREATE TABLE events (
    sequence    bigserial PRIMARY KEY,
    stream_id   uuid NOT NULL,
    stream_seq  integer NOT NULL CHECK (stream_seq >= 1),
    event_type  text NOT NULL,
    payload     jsonb NOT NULL,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (stream_id, stream_seq)
);

CREATE INDEX events_by_stream ON events (stream_id, stream_seq);

CREATE FUNCTION lulan_events_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'events are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER events_append_only
    BEFORE UPDATE OR DELETE ON events
    FOR EACH ROW EXECUTE FUNCTION lulan_events_append_only();

-- Outbox: written in the same transaction as the event; a relay task
-- delivers rows to the configured EventSink and marks them.
CREATE TABLE outbox (
    id             bigserial PRIMARY KEY,
    event_sequence bigint NOT NULL REFERENCES events (sequence),
    delivered_at   timestamptz
);

CREATE INDEX outbox_undelivered ON outbox (id) WHERE delivered_at IS NULL;

-- Orders read model. `status` is derived state — the event stream
-- (stream_id = order id) is the source of truth; replay must reproduce it.
CREATE TABLE orders (
    id                uuid PRIMARY KEY,
    trip_id           uuid NOT NULL REFERENCES trips (id),
    passenger_name    text NOT NULL,
    status            text NOT NULL CHECK (status IN (
        'draft', 'locked', 'pending_payment', 'paid', 'ticketed',
        'boarded', 'completed', 'cancelled', 'expired', 'refunded')),
    total_minor       bigint NOT NULL DEFAULT 0,
    currency          text NOT NULL DEFAULT 'PHP',
    payment_intent_id text UNIQUE,
    -- Claims are provisional until Paid: the sweeper expires and releases
    -- orders that never complete payment (cleared on PaymentCaptured).
    expires_at        timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX orders_due_for_expiry ON orders (expires_at)
    WHERE status IN ('locked', 'pending_payment');

CREATE TABLE order_items (
    order_id   uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    unit_id    uuid NOT NULL REFERENCES capacity_units (id),
    unit_code  text NOT NULL,
    kind       text NOT NULL CHECK (kind IN ('seat', 'pool')),
    from_index smallint NOT NULL,
    to_index   smallint NOT NULL,
    quantity   integer NOT NULL DEFAULT 1 CHECK (quantity > 0),
    PRIMARY KEY (order_id, unit_id)
);
