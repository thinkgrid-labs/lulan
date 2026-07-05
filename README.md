# Lulan — Open-Source Headless Reservation Engine for Transit & Capacity Booking

[![CI](https://github.com/thinkgrid-labs/lulan/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/lulan/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/core-AGPL--3.0-blue.svg)](LICENSE)
[![SDKs: MIT](https://img.shields.io/badge/SDKs%20%26%20validators-MIT-green.svg)](#license)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

**Lulan is an open-source, API-first reservation system for buses, ferries, rail, and any operator that sells capacity instead of products.** It is a headless booking engine written in Rust: segment-aware seat inventory, race-free reservations under high concurrency, event-sourced order lifecycle, a sandboxed WebAssembly pricing engine, and cryptographically signed QR tickets that validate **fully offline**.

Think "Medusa or commercetools, but for seats, cabins, vehicle slots, and cargo holds" — inventory that exists in **space and time**, not on a shelf.

---

## Table of Contents

- [Why an open-source transit reservation system?](#why-an-open-source-transit-reservation-system)
- [Key features](#key-features)
- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [API overview](#api-overview)
- [Architecture](#architecture)
- [Benchmarks](#benchmarks)
- [Project status & roadmap](#project-status--roadmap)
- [Use cases](#use-cases)
- [License](#license)

---

## Why an open-source transit reservation system?

Most transportation operators still run on legacy reservation software: expensive licenses, proprietary lock-in, monolithic deployments, and booking conflicts the moment demand spikes. General-purpose e-commerce platforms don't help — they assume inventory is static. Transit inventory isn't:

```
A ─── B ─── C ─── D        Seat 12A on one sailing:

A → B   Reserved           the same physical seat is sold inventory
B → C   Available          on one journey segment and open capacity
C → D   Reserved           on the next.
```

Selling a seat means atomically claiming a **span of segments** on a **specific departure**, while hundreds of other buyers race you for the same span. Lulan is a reservation engine built for exactly that problem — and it is verified, not just claimed: the invariant harness fires **10,000 simultaneous contenders at 52 seats and records zero double-sells**, including with Redis killed mid-run.

## Key features

| Capability | What it means |
|---|---|
| **Segment-aware inventory** | Seats, cabins, vehicle decks, and cargo pools tracked per journey segment with bitmask occupancy — the PRD's "12A is free B→C but taken A→B" answered in one query. |
| **Race-free booking at scale** | Redis-backed soft holds + guarded PostgreSQL claims. Zero double-sells at 10k concurrent contenders (chaos-tested). |
| **Event-sourced orders** | Every order is an append-only event stream (`order_created → … → passenger_boarded`) with a transactional outbox; replaying events reproduces the read model exactly. |
| **Multi-passenger itineraries** | One order, N passengers; per-passenger seats and fares, including legally mandated senior/PWD/child discounts (PH-market ready). |
| **Pluggable pricing (WASM)** | Deterministic integer-only fare rules, plus operator-supplied pricing modules run in a fuel-metered, memory-capped WebAssembly sandbox with **no host imports** — measured at p95 ≈ 443 µs per quote. Native and WASM engines are property-tested to be bit-identical. |
| **Signed quotes** | Short-TTL HMAC quote tokens: the price shown is the price charged, tamper-proof. |
| **Offline ticket validation** | Ed25519-signed CBOR QR tickets (~245 bytes). The MIT `lulan-validate` crate verifies them with no server, no clock assumptions, and compiles to WebAssembly for browser and mobile conductor apps. |
| **Offline boarding sync** | Conductor devices journal scans locally and sync idempotent batches when connectivity returns; duplicate and cloned-QR scans are detected and flagged. |
| **Headless & self-hostable** | JSON REST APIs only — bring your own storefront, kiosk, or POS. One Docker image, PostgreSQL + Redis, no per-booking fees. |

## How it works

A booking flows through five stages, each independently verifiable:

1. **Search & availability** — `GET /v1/trips/search` answers span-aware availability (per-segment seat and pool occupancy).
2. **Quote** — `POST /v1/quotes` prices the itinerary (per passenger type, occupancy, peak day, promos) and returns a signed, short-lived quote token.
3. **Order** — `POST /v1/orders` atomically claims every item for N passengers; any conflict rolls back everything.
4. **Pay & ticket** — a payment-provider webhook captures payment and auto-issues one Ed25519-signed QR ticket per passenger.
5. **Board — even offline** — gate devices verify tickets locally against cached public keys (`GET /v1/ticket-keys`), then sync their scan journal (`POST /v1/scans`); the order aggregates to *Boarded* when the last passenger scans in.

## Quick start

Prerequisites: Rust (edition 2024), Docker, [`just`](https://github.com/casey/just).

```bash
git clone https://github.com/thinkgrid-labs/lulan.git
cd lulan
just up      # PostgreSQL 16 + Redis 7 (Docker)
just serve   # migrate, seed the demo ferry network, serve on :8080
```

Book a ticket end to end:

```bash
# 1. Find a sailing (demo network: Batangas → Caticlan → Iloilo → Cebu)
curl "localhost:8080/v1/trips/search?origin=BTG&destination=CEB&date=$(date +%F)"

# 2. Quote a senior + child itinerary
curl -X POST localhost:8080/v1/quotes -H 'content-type: application/json' -d '{
  "trip_id": "<trip>",
  "items": [
    {"unit_code": "12C", "origin": "BTG", "destination": "CEB", "passenger_type": "senior"},
    {"unit_code": "12D", "origin": "BTG", "destination": "CEB", "passenger_type": "child"}
  ]}'

# 3. Order with the quote token, pay (fake provider), fetch signed QR tickets
curl -X POST localhost:8080/v1/orders ...
curl -X POST localhost:8080/v1/orders/<id>/payment
curl localhost:8080/v1/orders/<id>/tickets
```

Run the test suite and the concurrency harness:

```bash
just check                 # fmt + clippy + full test suite
just loadgen 10000 0.5     # 10k contenders, 50% via holds — expect 0 double-sells
```

## API overview

| Endpoint | Purpose |
|---|---|
| `GET /v1/trips/search` | Trip search with span-aware availability |
| `GET /v1/trips/{id}/availability` | Per-seat / per-pool availability for a journey span |
| `POST /v1/trips/{id}/holds` | Soft-hold a seat span (TTL, Redis) |
| `POST /v1/quotes` | Itemised fare quote + signed quote token |
| `POST /v1/orders` | Atomic multi-passenger booking (live-priced or quote-token) |
| `POST /v1/orders/{id}/payment` | Create payment intent (provider port) |
| `POST /v1/payments/fake/webhook` | Idempotent capture webhook → auto-issues tickets |
| `GET /v1/orders/{id}/tickets` | Ed25519-signed QR ticket tokens |
| `GET /v1/ticket-keys` | Public keys for offline validators |
| `POST /v1/scans` | Batched, idempotent boarding-scan sync |

OpenAPI spec and a generated TypeScript SDK are next on the roadmap (Phase 6).

## Architecture

```
        Your storefront / kiosk / POS / conductor app
                          │
              JSON REST  (TypeScript SDK planned)
                          │
                ┌─────────▼──────────┐
                │     lulan-api      │  Axum · HTTP/2
                ├────────────────────┤
                │    lulan-engine    │  inventory · orders · tickets
                │   lulan-pricing    │  native + WASM sandbox (wasmtime)
                └─┬───────┬────────┬─┘
                  │       │        │
            PostgreSQL  Redis   *.wasm pricing modules
            (truth +   (holds,  (operator-supplied,
             events)    cache)   no host imports)

   Offline edge:  lulan-validate (MIT, wasm32) verifies tickets
                  with zero server dependency.
```

Design decisions are documented as ADRs in [`docs/adr/`](docs/adr/) — including why the event log lives in PostgreSQL rather than Kafka, why claims are guarded SQL updates rather than Redis locks, and why pricing is a trait first and WASM second. The full build plan lives in [`docs/development-plan.md`](docs/development-plan.md).

## Benchmarks

Real numbers, adversarial shapes, published in [`docs/benchmarks.md`](docs/benchmarks.md):

- **0 double-sells** across 10,000 simultaneous contenders on one 52-seat vessel — repeated with Redis killed mid-run (chaos test).
- **WASM pricing**: p50 347 µs / p95 443 µs per quote including per-call instantiation (PRD target < 5 ms), enforced by a CI assertion.
- **Ticket QR payload**: ~245 bytes signed (target < 400 bytes for low-error-correction QR).

## Project status & roadmap

> ⚠️ **Pre-1.0, active development.** APIs may change until the first stable release. Verified working today — the checked items below are implemented and covered by the test suite.

- [x] Segment-aware inventory engine (seats, pools, span claims)
- [x] Soft holds + race-free claims (0 double-sells @ 10k contenders)
- [x] Event-sourced order lifecycle with payment-provider port
- [x] Pricing engine — native + sandboxed WASM modules, signed quotes
- [x] Multi-passenger orders with passenger-type fares
- [x] Ed25519 QR ticketing + offline validation (`lulan-validate`)
- [x] Offline boarding-scan sync with replay/clone detection
- [ ] Webhooks for operator integrations
- [ ] Authentication: API keys, identity-provider port, guest checkout
- [ ] OpenAPI spec + generated TypeScript SDK (`@lulan/storefront-sdk`)
- [ ] Observability: OpenTelemetry traces, Prometheus metrics
- [ ] Reference Next.js storefront + React Native conductor app
- [ ] `@lulan/validate` npm package (WASM build of the validator)

## Use cases

Lulan models any business that reserves **capacity over space and time**: provincial bus lines, inter-island ferries and RoRo vessels, regional airlines, rail and commuter networks, shuttle and van fleets, cargo and parcel space, vehicle-deck slots, parking, and timed-entry ticketing.

## Contributing

Lulan is developed in the open and welcomes issues, design discussions, and pull requests. Start with the [development plan](docs/development-plan.md) and ADRs to understand where the project is heading. Contribution guidelines will formalise as the project approaches its first release.

## License

| Package | License | Why |
|---|---|---|
| `lulan-engine`, `lulan-api`, `lulan-pricing` (host) | **AGPL-3.0** | The core stays open — improvements to hosted deployments flow back. |
| `lulan-validate` (offline ticket verification) | **MIT** | Embed it in proprietary conductor and kiosk apps freely. |
| `lulan-pricing-guest` (reference WASM pricing module) | **MIT** | Copy it as the starting point for your own fare engine. |
| SDKs, UI components, examples | **MIT** | Build commercial storefronts without restriction. |

---

**Keywords**: open-source reservation system · headless booking engine · transit ticketing · bus booking system · ferry reservation software · seat reservation API · segment inventory · Rust booking engine · offline ticket validation · QR ticketing · WebAssembly pricing

*Lulan (Filipino/Tagalog: "to board, to load") aims to be the open-source foundation for capacity reservation — bringing modern developer tooling to an industry still dominated by legacy software. Transportation is only the beginning.*
