-- Not every pool is people. A pool sells capacity by a count, but that
-- count means different things: general admission and ferry foot
-- passengers are one admitted person per unit (each needs a boarding
-- pass), while cargo kilograms and vehicle-deck slots are bulk capacity
-- (the quantity is weight or vehicles, not passengers, and issuing one QR
-- per unit would be nonsense).
--
-- `admission` marks the first kind. Only admission pools issue one bearer
-- ticket per unit claimed. The default is false so existing bulk pools
-- (CARGO_KG, VEHICLE_DECK) keep their current behaviour: no per-unit
-- tickets.
ALTER TABLE capacity_units
    ADD COLUMN admission boolean NOT NULL DEFAULT false;
