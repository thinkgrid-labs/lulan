-- Phase 7.5: staff RBAC and admin operations.
--
-- Staff are IdP identities with operator roles — never accounts in core
-- (same principle as customers: we verify identity, we don't own it).
-- Three principal types now exist, nominally distinct:
--   customers (travellers, self-enrolled) · staff (operator humans,
--   enrolled by an admin) · api_keys (machines).

CREATE TABLE staff (
    id           uuid PRIMARY KEY,
    issuer       text NOT NULL,
    subject      text NOT NULL,
    email        text,
    display_name text NOT NULL,
    -- admin: everything · ops: network/schedules/fares ·
    -- support: orders/refunds/manifests
    role         text NOT NULL CHECK (role IN ('admin', 'ops', 'support')),
    active       boolean NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

-- Which HUMAN did it — regulatory expectation in most transport markets.
ALTER TABLE audit_log ADD COLUMN staff_id uuid REFERENCES staff (id);

-- Trip lifecycle for admin cancellation. Search only surfaces scheduled
-- trips; cancellation cascades to affected orders (cancel unpaid, refund
-- paid) in the application layer.
ALTER TABLE trips ADD COLUMN status text NOT NULL DEFAULT 'scheduled'
    CHECK (status IN ('scheduled', 'cancelled'));
