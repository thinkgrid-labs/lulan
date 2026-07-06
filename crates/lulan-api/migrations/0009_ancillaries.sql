-- Ancillaries: operator-defined add-ons (baggage, meals, insurance,
-- priority boarding — anything the operator wants to sell alongside the
-- fare). Deliberately NOT capacity: scarce things are capacity units
-- (extra-legroom = a seat, baggage kilos = the CARGO_KG pool). These are
-- flat-priced SKUs that ride along as order lines.

CREATE TABLE ancillaries (
    id          uuid PRIMARY KEY,
    code        text NOT NULL UNIQUE,
    name        text NOT NULL,
    description text NOT NULL DEFAULT '',
    -- Free-form grouping for storefront display (baggage, meal, insurance…)
    kind        text NOT NULL,
    price_minor bigint NOT NULL CHECK (price_minor >= 0),
    -- 'passenger': one per passenger (a meal, an insurance policy).
    -- 'order': one per order (e.g. carbon offset).
    per         text NOT NULL DEFAULT 'passenger' CHECK (per IN ('passenger', 'order')),
    -- 'journey': tied to one leg (meal on the outbound).
    -- 'itinerary': covers the whole booking (travel insurance).
    scope       text NOT NULL DEFAULT 'itinerary' CHECK (scope IN ('journey', 'itinerary')),
    active      boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Purchased add-ons. Catalog fields are SNAPSHOTTED (code/name/price at
-- purchase time) so later catalog edits never rewrite sold orders.
CREATE TABLE order_ancillaries (
    id           uuid PRIMARY KEY,
    order_id     uuid NOT NULL REFERENCES orders (id),
    ancillary_id uuid NOT NULL REFERENCES ancillaries (id),
    code         text NOT NULL,
    name         text NOT NULL,
    -- Set for journey-scoped lines (which leg the meal is served on).
    trip_id      uuid REFERENCES trips (id),
    -- Set for per-passenger lines.
    passenger_id uuid REFERENCES passengers (id),
    quantity     integer NOT NULL DEFAULT 1 CHECK (quantity > 0),
    total_minor  bigint NOT NULL CHECK (total_minor >= 0)
);

CREATE INDEX order_ancillaries_by_order ON order_ancillaries (order_id);
