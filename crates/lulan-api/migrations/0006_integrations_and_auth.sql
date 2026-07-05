-- Phase 6: operator integrations (webhooks), API-key auth, customer
-- identity references, idempotency, and the admin audit log.

-- Operator-registered webhook destinations. Empty event_types = all.
CREATE TABLE webhook_endpoints (
    id          uuid PRIMARY KEY,
    url         text NOT NULL,
    secret      text NOT NULL,
    event_types text[] NOT NULL DEFAULT '{}',
    active      boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- One row per (event, endpoint): the durable delivery queue. The outbox
-- relay fans events out into pending rows; a worker POSTs them with
-- exponential backoff until delivered or attempts are exhausted.
CREATE TABLE webhook_deliveries (
    id              bigserial PRIMARY KEY,
    endpoint_id     uuid NOT NULL REFERENCES webhook_endpoints (id) ON DELETE CASCADE,
    event_sequence  bigint NOT NULL REFERENCES events (sequence),
    status          text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts        int NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    last_error      text,
    delivered_at    timestamptz,
    UNIQUE (endpoint_id, event_sequence)
);

CREATE INDEX webhook_deliveries_due
    ON webhook_deliveries (next_attempt_at)
    WHERE status = 'pending';

-- Server-to-server credentials. The plaintext key is shown once at
-- creation; only its SHA-256 lives here.
CREATE TABLE api_keys (
    id         uuid PRIMARY KEY,
    key_hash   bytea NOT NULL UNIQUE,
    label      text NOT NULL,
    role       text NOT NULL
        CHECK (role IN ('operator_admin', 'integration', 'validator')),
    active     boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Customer *references*, not accounts: identity lives in the operator's
-- IdP; we record (issuer, subject) so orders can be listed per customer.
CREATE TABLE customers (
    id         uuid PRIMARY KEY,
    issuer     text NOT NULL,
    subject    text NOT NULL,
    email      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

-- NULL customer_id = guest checkout (first-class). Guests must leave a
-- contact for retrieval and payment reconciliation.
ALTER TABLE orders ADD COLUMN customer_id uuid REFERENCES customers (id);
ALTER TABLE orders ADD COLUMN guest_contact text;
CREATE INDEX orders_by_customer ON orders (customer_id) WHERE customer_id IS NOT NULL;

-- Booking retries must not double-charge: the first 201 response for a
-- key is stored and replayed verbatim for duplicates.
CREATE TABLE idempotency_keys (
    key         text PRIMARY KEY,
    order_id    uuid REFERENCES orders (id),
    status_code int NOT NULL,
    response    jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Who did what with which admin credential.
CREATE TABLE audit_log (
    id         bigserial PRIMARY KEY,
    api_key_id uuid REFERENCES api_keys (id),
    action     text NOT NULL,
    detail     jsonb NOT NULL DEFAULT '{}',
    at         timestamptz NOT NULL DEFAULT now()
);
