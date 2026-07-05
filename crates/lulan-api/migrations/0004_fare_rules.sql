-- Fare rules (Phase 4): the serialized FareRuleSet pricing engines
-- evaluate. Global scope for v1 (newest active row wins); per-route
-- scoping can be added later without breaking the shape.
CREATE TABLE fare_rules (
    id         uuid PRIMARY KEY,
    active     boolean NOT NULL DEFAULT true,
    rules      jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX fare_rules_active ON fare_rules (created_at DESC) WHERE active;

-- Each order item now records the price it was sold at.
ALTER TABLE order_items ADD COLUMN price_minor bigint NOT NULL DEFAULT 0;
