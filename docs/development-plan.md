# Lulan — Development Plan

**Against:** PRD Draft v2 ([PRD.md](./PRD.md))
**Status:** Proposed
**Date:** 2026-07-05

This plan turns the PRD into a sequenced build. It is organized as phases with explicit exit criteria, so each phase ships something demonstrable and the double-sell guarantee (the product's core promise) is proven early — before the surface area grows.

---

## 0. Guiding decisions (deviations from PRD, made explicit)

These are deliberate simplifications of the PRD architecture for v1. Each preserves the PRD's end state while cutting risk from the critical path.

| # | PRD says | v1 plan | Rationale |
|---|---|---|---|
| D1 | Kafka/Redpanda event store | **Postgres append-only event log + transactional outbox.** Kafka/Redpanda becomes an optional *sink* behind an `EventSink` trait. | The PRD's own "Small Operators" tier is Compose + Postgres + Redis. If the core *requires* Kafka, that tier doesn't exist. An outbox gives identical ordering/immutability guarantees at v1 scale, and events can be replayed into Redpanda later without data loss. |
| D2 | Redis for distributed locking | **Postgres is the source of truth for inventory claims; Redis holds only the fast-path soft hold (TTL).** | A Redis-only lock cannot guarantee "zero duplicate reservations" across failover (Redlock caveats are well known). Correctness must be anchored in a DB constraint that makes a double-sell *unrepresentable*, with Redis as a latency/UX optimization layer on top. |
| D3 | Pricing runs as WASM at the edge | **Pricing is a `PricingEngine` trait from day one; the first two implementations are native Rust and in-process WASM (wasmtime). Edge *deployment* is post-v1.** | The stable contract is the interface, not the runtime. In-process WASM proves the plugin model and sandboxing; pushing modules to edge PoPs is a distribution problem that shouldn't block the reservation core. |
| D4 | GraphQL / gRPC | Explicitly out of scope for v1 (matches PRD "future consideration"). | REST + OpenAPI + webhooks covers all three target users. |
| D5 | Multi-vertical capacity model | Model the domain as **generic capacity primitives** (resource → capacity units → segment spans), with transit as the first profile — but only build transit features. | Cheap now, very expensive to retrofit. This is how the §20 vision stays reachable without v1 scope creep. |

Each of these should be captured as an ADR in `docs/adr/` during Phase 0.

---

## 1. Repository layout

Monorepo: Cargo workspace for the engine, pnpm workspace for the TS ecosystem.

Crate count is kept deliberately low to start — splits inside a Cargo workspace are cheap to do later, so a boundary earns its own crate only when it has a hard technical reason (compilation target, licensing, or "must not link into the server"). Domain types, inventory, events, orders, and ticketing start life together in `lulan-engine`: they share transactions anyway (claims and order events commit atomically), and they can be split out once the seams prove real.

```
lulan/
├── Cargo.toml                    # Rust workspace root
├── package.json                  # pnpm workspace root
├── Dockerfile                    # builds the lulan-api image (see "Docker packaging" below)
├── .dockerignore                 # excludes packages/, apps/, docs/ from the build context
├── crates/
│   ├── lulan-engine/             # domain types, state machines, segment inventory,
│   │                             #   event log/outbox, orders, ticket issuance & signing
│   ├── lulan-pricing/            # PricingEngine trait, native + wasmtime hosts
│   ├── lulan-validate/           # offline QR verification core — separate crate because it
│   │                             #   compiles to WASM and is MIT-licensed (see below)
│   ├── lulan-api/                # Axum server binary: REST, auth, OpenAPI (utoipa),
│   │                             #   webhooks, config, migrations, telemetry
│   └── lulan-loadgen/            # concurrency invariant checker + benchmark harness
│                                 #   (own crate so it never links into the server)
├── packages/
│   ├── sdk/                      # @lulan/sdk — generated from OpenAPI
│   ├── validate/                 # @lulan/validate — WASM build of lulan-validate
│   └── ui/                       # @lulan/ui — seat map, schedule search, etc. (later)
├── apps/                         # reference apps (Phase 8)
│   ├── storefront/               # Next.js
│   └── conductor/                # React Native / Expo
├── deploy/
│   ├── compose/                  # docker-compose for small operators + dev
│   └── k8s/                      # Helm chart (later)
├── docs/
│   ├── PRD.md
│   ├── development-plan.md
│   └── adr/
├── LICENSE-AGPL                  # crates/ (except lulan-validate)
└── LICENSE-MIT                   # lulan-validate, packages/, apps/, deploy/, docs
```

Likely later splits (when warranted, not before): `lulan-domain` out of `lulan-engine` once a second consumer needs pure types without sqlx; `lulan-events` once the Redpanda sink lands.

Licensing boundary (PRD §18) maps cleanly to directories: everything under `crates/` is AGPL-3.0, everything under `packages/` and `apps/` is MIT. **Exception:** `lulan-validate` must be MIT (or dual-licensed) — an AGPL core inside `@lulan/validate` would virally capture every proprietary conductor app that embeds it, which contradicts PRD §18's intent for client libraries. This licensing exception is one of the two reasons it is a separate crate.

### Docker packaging

One image is shipped: **`lulan-api`**. The build context is the workspace root — `lulan-api` path-depends on `lulan-engine`, `lulan-pricing`, and `lulan-validate`, so the builder stage needs all of `crates/` plus the root `Cargo.toml`/`Cargo.lock` to compile — but only the API binary is built and only it lands in the final image:

```dockerfile
# builder: full workspace context, single -p target
FROM rust:1-bookworm AS chef        # cargo-chef for dependency layer caching
# ... chef prepare / chef cook ...
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release -p lulan-api

# runtime: just the binary + migrations
FROM debian:bookworm-slim           # or gcr.io/distroless/cc
COPY --from=builder /app/target/release/lulan-api /usr/local/bin/
EXPOSE 8080
ENTRYPOINT ["lulan-api"]
```

Key points:

- `.dockerignore` excludes `packages/`, `apps/`, `docs/`, `target/`, `node_modules/` — the TS ecosystem never enters the engine's build context, keeping builds fast and the image honest about what it contains.
- `cargo build -p lulan-api` compiles only the API binary and its dependency graph; `lulan-loadgen` is skipped even though it's in the context.
- Dependency caching via `cargo-chef` (or `--mount=type=cache` on the cargo registry/target dirs) so schedule-of-day rebuilds don't recompile the world.
- WASM pricing modules are **runtime artifacts, not image layers** — loaded from a mounted volume/object store path, so operators update pricing without rebuilding the image (this is the PRD's "instant pricing updates" claim in deployment terms).
- Reference apps (`apps/storefront`, `apps/conductor`) get their own Dockerfiles in their own directories in Phase 8, if and when they're deployed; they are consumers of the API image, never part of it.

---

## 2. Phases

### Phase 0 — Foundation (repo, CI, ADRs)

**Goal:** a contributor can clone, `docker compose up`, and run tests in under 10 minutes.

- Cargo + pnpm workspace scaffolding as above; `rust-toolchain.toml`, `.editorconfig`, `justfile` (or `cargo xtask`) for dev commands.
- `deploy/compose/dev.yml`: Postgres 16, Redis 7. That's the whole dev stack (per D1/D2).
- Root `Dockerfile` + `.dockerignore` per the Docker packaging section above; CI builds the image so it never rots.
- CI (GitHub Actions): fmt + clippy (`-D warnings`) + test + `sqlx prepare` check; pnpm build/typecheck; license-header check per directory.
- ADRs 0001–0005 covering D1–D5.
- CONTRIBUTING.md, dual-license notices, issue templates.

**Exit criteria:** green CI on a hello-world Axum server with `/health/live` + `/health/ready`, migrations framework (sqlx migrate) in place.

---

### Phase 1 — Domain model & segment inventory (the heart)

**Goal:** correctly answer "is seat 12A available from B→C on trip T?" — the query every other feature depends on.

**Domain entities** (`lulan-engine::domain`):

- `Location` (stop/port/airport), `Route` (ordered locations), `TripPattern`, `Trip` (dated instance of a pattern).
- `Segment` = consecutive location pair within a trip; a passenger journey is a **span** of segments `[from_idx, to_idx)`.
- `Resource` (vehicle/vessel with a layout), `CapacityUnit` — the generic reservable thing (D5): a `Seat` (identity-based, spatial) or a `CapacityPool` (count/weight-based: cargo kg, vehicle deck slots, standing room). Ancillaries (meals, priority boarding) are pools too.
- `Hold`, `Claim`, `Order`, `Ticket` as distinct concepts (defined here, implemented in later phases).

**Segment availability representation** (`lulan-engine::inventory`):

- Per `(trip, capacity_unit)`, occupancy is a **segment bitmask** (`u64` covers 64 segments — more than any real-world trip; enforce a limit and document it). Seat 12A occupied A→B and C→D on a 3-segment trip = `0b101`.
- Availability check = `mask & span_mask == 0`. This makes the PRD §2 example a one-line AND.
- Pools use per-segment counters (`i32[]` or a `remaining[]` array column) with the same span semantics: available iff `min(remaining[from..to]) >= qty`.

**Postgres schema:** routes/trips/resources/capacity_units + `seat_occupancy(trip_id, unit_id, occupied_mask)` and `pool_occupancy(trip_id, pool_id, remaining int[])`. Search/read models denormalized for trip search.

**Deliverables:** exhaustive property tests (proptest) on span/mask algebra — overlapping spans, adjacent spans, full-trip spans; trip search query (origin, destination, date → trips + availability per fare class).

**Exit criteria:** given seeded schedules, the availability API answers segment-availability queries correctly under property-based testing. No locking yet.

---

### Phase 2 — High-concurrency holds & claims (the core promise)

**Goal:** zero duplicate reservations, proven under adversarial load. This phase is the product; everything else is packaging.

**Two-tier design (per D2):**

1. **Soft hold (Redis, fast path):** `SET lock:{trip}:{unit}:{span-key} {hold_id} NX PX {ttl}` — but span-aware: a Lua script that checks a per-unit hash of held masks and sets atomically (same pattern as your helpdeck sliding-window Lua, applied to bitmasks). Sub-millisecond, gives the "seat is being held for you" UX with countdown TTL (default 10 min, configurable).
2. **Hard claim (Postgres, source of truth):** at checkout, one transaction executes `UPDATE seat_occupancy SET occupied_mask = occupied_mask | $span WHERE trip_id=$1 AND unit_id=$2 AND (occupied_mask & $span) = 0` — rows affected = 0 ⇒ conflict ⇒ abort. Pools: `UPDATE ... SET remaining = remaining - qty WHERE min-check` equivalent. A `CHECK` constraint keeps counters non-negative. **Even if Redis lies, is flushed, or fails over, Postgres cannot double-sell.**

- Hold expiry: TTL in Redis + a sweeper task that releases expired holds' claims if payment never arrived (claims made at checkout carry an expiry until `Paid`).
- Fairness/stampede control: per-trip Redis token bucket in front of hold acquisition for holiday-spike behavior (degrade to queueing, not errors).

**Proof, not vibes:** build `lulan-loadgen` *in this phase*. Scenario: N=10k concurrent clients target the same 50-seat trip; invariant checker asserts exactly 50 claims, zero overlap, all others got clean conflict responses. Run in CI (scaled down) and as a releasable benchmark (full scale). This also produces the first honest numbers against PRD §14 targets.

**Exit criteria:** invariant suite passes at 10k+ concurrent contenders; chaos variants pass (kill Redis mid-run, restart Postgres connection pool mid-run — no double-sell in any case); p95 hold latency measured and published.

---

### Phase 3 — Event log & order engine

**Goal:** the order lifecycle state machine with an immutable audit trail.

- `lulan-engine::events`: append-only `events` table — `(sequence bigserial, stream_id, stream_seq, event_type, payload jsonb, occurred_at)`, unique `(stream_id, stream_seq)` for optimistic concurrency. **Insert-only enforced in Postgres** (`REVOKE UPDATE, DELETE` + trigger), since immutability is a PRD security claim (§10).
- Transactional outbox + relay task; `EventSink` trait with `WebhookSink` (Phase 5) and feature-gated `RedpandaSink` (post-v1 or stretch).
- Order state machine in `lulan-engine::domain` as a typed transition function (`Draft → Locked → PendingPayment → Paid → Ticketed → Boarded → Completed`, plus `Cancelled`/`Expired`/`Refunded`); illegal transitions unrepresentable, every legal transition emits exactly one event. Claims from Phase 2 are executed *in the same transaction* as the `SeatLocked`/order events — one transaction, one truth.
- **Payments as a port, not a feature:** `PaymentProvider` trait with intent/authorize/capture/refund + webhook reconciliation; ship a `FakeProvider` (dev) and one real adapter (suggest Stripe first; a PH-relevant one like Xendit/PayMongo as a fast follow given the target market). Reconciliation job for the PRD's "failed payment reconciliation" pain point.

**Exit criteria:** full lifecycle drivable via API against FakeProvider; event log replays to identical read-model state; payment webhook out-of-order/duplicate delivery handled idempotently.

---

### Phase 4 — Pricing engine

**Goal:** replaceable pricing per D3.

- `PricingEngine` trait: `(journey, fare_class, occupancy snapshot, time, passenger attrs) → PriceQuote` (money as minor units + currency — no floats).
- Impl 1: **native rule engine** — fare classes, date/peak rules, occupancy tiers, promo codes; rules stored in Postgres, cached.
- Impl 2: **wasmtime host** — WIT-defined interface, fuel-metered, memory-capped, pure function (no host I/O). This *is* the plugin sandbox story for §9, delivered early.
- Quote integrity: quotes are signed + short-TTL so checkout can verify price without recompute; repricing on expiry.
- Benchmark: `<5 ms` PRD target measured in-process (the honest v1 claim; "edge" latency is a deployment property, post-v1).

**Exit criteria:** same fare rules produce identical results via native and WASM engines (differential testing); a third-party can write a pricing module against the WIT file alone.

---

### Phase 5 — Ticketing, signed QR & offline validation

**Goal:** the ground-staff story — boarding continues with zero connectivity.

- `lulan-engine::ticket`: on `Paid → Ticketed`, issue ticket with **Ed25519-signed compact payload** (CBOR or CWT-style: ticket id, trip, unit/span, passenger hash, fare class, validity window, key id, signature) → QR. Target < ~400 bytes for low-error-correction QR scanning on cheap devices.
- Key management: rotating keypairs, `kid` in payload, JWKS-style public-key distribution endpoint; devices cache keysets.
- `lulan-validate`: **pure Rust verification core** (signature check, validity window, trip match) with no server dependency — compiled to WASM for `@lulan/validate` (browser + RN) and usable natively. One verification implementation everywhere.
- Offline replay prevention (per PRD §10, honestly scoped): a signature can't stop a *cloned* QR across two offline devices — that's physics. Mitigations: device-local seen-set (rejects re-scan on same device), scan-event journal synced when connectivity returns, server-side conflict detection flags duplicates post-hoc. Document this threat model explicitly.
- Boarding sync: batched idempotent `PassengerBoarded` scan-event upload; server merges into event log.
- Webhooks (`WebhookSink`): HMAC-signed deliveries, retries with backoff, per-endpoint event filtering — closes out §8/§9 integration surface.

**Exit criteria:** demo script — book → pay (fake) → ticket → QR rendered → validated by `@lulan/validate` in a browser with network disabled → scan events sync afterward and appear in the event log.

---

### Phase 6 — API hardening, auth & OpenAPI/SDK

**Goal:** the DX promise: typed, versioned, generatable.

- Auth: API keys for server-to-server, JWT (RS256/EdDSA) for user-context calls, OAuth2 client-credentials; RBAC roles (`operator_admin`, `agent`, `conductor`, `integration`) enforced at the Axum extractor layer.
- OpenAPI via `utoipa` annotations, spec committed and diff-checked in CI (breaking-change detection = the API versioning story). Path versioning `/v1/...`.
- `@lulan/storefront-sdk` generated from the spec (openapi-ts or similar), published to npm; smoke-tested in CI against a live compose stack.
- Rate limiting (per-key sliding window — port the pattern from helpdeck), idempotency keys on all mutating endpoints (critical for booking retries), audit log on admin mutations.
- Observability (§11): `tracing` + OpenTelemetry OTLP export, Prometheus `/metrics`, per-phase spans across hold→claim→pay→ticket. This lands here but instrumentation is added incrementally from Phase 2 (the load harness needs it).

**Exit criteria:** an external developer can build a working booking flow from README + SDK + OpenAPI docs without reading engine source.

---

### Phase 7 — Deployment, benchmarks & migration tooling

**Goal:** the operator story.

- `deploy/compose/production.yml` — the Small Operator tier: server, Postgres, Redis, Caddy. One-command install, documented backup/restore.
- Helm chart for the Kubernetes tier; HPA notes; Postgres HA left to operator choice (document CloudNativePG/RDS patterns rather than owning them).
- Published benchmark suite + results vs. PRD §14 table (adjusted targets if reality disagrees — publish real numbers, they're a credibility asset for an infra project).
- Migration tooling (§15, minimum viable): CSV importers for schedules/passengers **plus a GTFS/GTFS-Flex importer** — GTFS is the lingua franca of transit schedules and worth naming in the PRD; instant compatibility with existing operator data. Legacy adapters/dual-write are post-v1.

**Exit criteria:** clean-machine install to first booking in < 30 minutes following docs only.

---

### Phase 8 — Reference apps & launch

**Goal:** the "Medusa moment" — people evaluate infra through its demo apps.

- `@lulan/ui`: seat map (the hero component — segment-aware rendering), schedule search, passenger form, ticket viewer, QR scanner wrapper.
- Next.js storefront (book a ferry/bus trip end-to-end), Expo conductor app (offline validation showcase — this demos the sharpest differentiator), minimal admin (schedules, trips, fares, manifests, refunds).
- API playground = hosted docs (Scalar/Redoc) against a seeded demo instance.
- Launch assets: docs site, architecture deep-dive post ("how Lulan guarantees no double-sell"), benchmark post, seeded demo data (a fictional PH inter-island ferry network is on-brand and demos segments naturally: Batangas → Caticlan → Iloilo → Cebu).

**Exit criteria:** public repo, v0.1.0 tagged, demo deployed, quickstart verified by someone who isn't you.

---

## 3. Sequencing & effort

Phases 0–2 are strictly sequential (each builds on the last). After Phase 3, work can parallelize.

| Phase | Effort (focused solo dev) | Cumulative milestone |
|---|---|---|
| 0 Foundation | ~1 wk | M0: repo + CI + compose |
| 1 Domain & inventory | 2–3 wk | M1: availability engine proven |
| 2 Holds & claims | 2–3 wk | **M2: double-sell impossible, benchmarked** ← de-risk point |
| 3 Events & orders | 3 wk | M3: full lifecycle w/ fake payments |
| 4 Pricing | 2 wk | M4: pluggable pricing (native + WASM) |
| 5 Ticketing & offline | 3 wk | M5: offline boarding demo |
| 6 API/auth/SDK | 2–3 wk | M6: SDK-driven integration |
| 7 Deploy & bench | 2 wk | M7: operator install story |
| 8 Reference apps | 3–4 wk | **v0.1.0 launch** |

Roughly **5–6 months solo**; Phases 4/5 and 6/7 pair well for parallelization if a second contributor joins. If early feedback matters, **M5 is the right point for a quiet source-available preview** — the offline demo is the wow moment even without polished apps.

## 4. Risks

| Risk | Mitigation |
|---|---|
| Correctness bug in span/mask algebra silently double-sells | Property tests in Phase 1; Postgres claim is a single guarded UPDATE (small audit surface); invariant-checking load harness in CI forever |
| PRD perf targets not met (50k concurrent locks, <20 ms) | Benchmark from Phase 2, not Phase 7; publish real numbers and adjust the PRD table — infra credibility comes from honest benchmarks |
| Scope creep toward legacy-PSS feature parity (interlining, GDS, codeshare) | v1 boundary = single-operator, own-inventory. Say so in README. |
| AGPL scares away integrators | Directory-level MIT for everything a client app links (see licensing note in §1 — `lulan-validate` especially) |
| Offline replay expectations exceed what crypto can deliver | Documented threat model in Phase 5; don't promise "replay prevention", promise "replay detection + same-device rejection" |
| Solo-dev burnout across 8 phases | Every phase exits with something demoable; M2 and M5 are natural "publish a post" checkpoints |

## 5. Explicitly out of scope for v1

Multi-operator marketplaces / interlining · GDS/OTA channel integration · GraphQL & gRPC · edge-deployed WASM (in-process only) · Kafka/Redpanda as a requirement (optional sink at most) · seat *assignment optimization* (auto-seating) · loyalty programs · multi-currency settlement (multi-currency *display* ok).
