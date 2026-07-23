# Launch post outline: "How Lulan guarantees no double-sell"

Audience: infra-curious engineers. Goal: earn trust through one correctness
claim, shown, not asserted. ~1200 words.

## Spine

1. **The trap (150w).** Transit inventory is a span of segments on a dated
   trip, not a row in a `products` table. Seat 12A sold A→B is still free
   B→C. The naive `SELECT available … then UPDATE` double-sells the instant
   two buyers race. Frame it as the problem generic e-commerce stacks
   quietly get wrong.

2. **The representation (200w).** Per `(trip, seat)` occupancy is a `u64`
   segment bitmask. Availability is `mask & span_mask == 0` — one AND.
   Show the PRD example (`0b101`). This is the idea the whole guarantee
   rests on: make a double-sell *representable* as a bit collision, then
   forbid the collision in one statement.

3. **The guarantee (300w) — the heart.** The claim is a single guarded
   UPDATE:
   `UPDATE seat_occupancy SET occupied_mask = occupied_mask | $span WHERE … AND (occupied_mask & $span) = 0`.
   Rows affected = 0 means someone owns part of the span; abort. There is
   no read-then-write window, no application-level lock, no Redlock caveat.
   Postgres cannot double-sell a segment even if every layer above it is
   wrong. Contrast explicitly with a Redis-only lock and why that fails
   across failover (ADR 0002).

4. **Redis is a UX layer, not the truth (150w).** Soft holds live in Redis
   for the "seat is being held for you" countdown. Losing Redis loses
   holds, never sold inventory. State it as a deliberate split: latency
   optimization on top, correctness anchored in the DB constraint.

5. **Proof, not vibes (250w).** The invariant harness: 10,000 concurrent
   contenders at 52 seats, assert exactly 52 claims and zero overlap,
   every loser gets a clean 409. It runs in CI scaled down and as a
   releasable benchmark. Then the chaos variant — kill Redis mid-run,
   restart the PG pool — still zero double-sells. Paste the harness output.
   Link the honest latency numbers (`docs/benchmarks.md`): p95 ~10ms at
   200/s, and the 500/s number we *missed* and published anyway.

6. **Close (100w).** Correctness you can run, not a claim you have to
   believe. Apache-2.0, one Docker image. Link QUICKSTART + the repo.

## Assets
- The bitmask diagram (reuse the README segment ASCII).
- The guarded-UPDATE snippet (`crates/lulan-engine/src/inventory/store.rs`).
- Harness output block; benchmark table.
