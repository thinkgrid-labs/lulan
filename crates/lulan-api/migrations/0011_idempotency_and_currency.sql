-- Two fixes to POST /v1/orders.
--
-- 1. Idempotency keys were global and written AFTER the order was created.
--    That failed in exactly the case the feature exists for: two concurrent
--    retries both missed the read and both booked. It also let any caller
--    replay another caller's stored response — which carries the order's
--    passenger names and its retrieval token, the credential for reading
--    and claiming that booking. Keys are now scoped to the caller, bound to
--    the request body, and RESERVED before the order is created.
--
--    The table is a short-lived dedup cache with no scope or request
--    fingerprint on its existing rows, so it is recreated rather than
--    backfilled with values that could never match.
--
-- 2. orders.currency defaulted to 'PHP' and the engine bound that constant
--    unconditionally, so an operator whose fare rules price in any other
--    currency got a quote in theirs and an order row in PHP. Currency now
--    comes from the ruleset (or the locked quote token) on every write, and
--    the column default is dropped so a missing value fails loudly instead
--    of silently becoming pesos.

DROP TABLE idempotency_keys;

CREATE TABLE idempotency_keys (
    -- Who the key belongs to: `customer:<uuid>` or `guest:<sha256(contact)>`.
    -- Without this, `Idempotency-Key: 1` from one client replays another's
    -- order.
    scope        text NOT NULL,
    key          text NOT NULL,
    -- SHA-256 of the canonical request. A key reused with a different body
    -- is a client bug, not a retry: it is refused rather than answered with
    -- an unrelated order.
    request_hash bytea NOT NULL,
    -- 'pending' = reserved, order in flight; 'completed' = response stored.
    status       text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'completed')),
    order_id     uuid REFERENCES orders (id),
    status_code  int,
    response     jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (scope, key)
);

-- The sweeper drops stale reservations (a crash between reserve and
-- complete would otherwise block that key forever) and expires old
-- responses so the cache cannot grow without bound.
CREATE INDEX idempotency_keys_by_age ON idempotency_keys (created_at);

ALTER TABLE orders ALTER COLUMN currency DROP DEFAULT;
