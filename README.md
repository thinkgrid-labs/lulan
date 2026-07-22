# Lulan — Open-source Headless Reservation Infrastructure for Modern Transit.

[![CI](https://github.com/thinkgrid-labs/lulan/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/lulan/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

**Lulan is an open-source, API-first reservation platform for airlines, buses, ferries, rail, and any operator that sells capacity instead of products.** It runs in two modes behind one API: a **complete standalone booking engine** — inventory, pricing, payments, QR ticketing, offline validation, end to end — or an **orchestration layer** that owns the customer-facing reservation experience and synchronizes confirmed bookings into the operational systems you already run (airline PSS, ferry manifest backend, bus dispatch) through sync connectors.

Built in Rust: segment-aware seat inventory, race-free reservations under high concurrency, event-sourced order lifecycle, a sandboxed WebAssembly pricing engine, and cryptographically signed QR tickets that validate **fully offline**.

Think of commerce tools, but for seats, cabins, vehicle slots, and cargo holds" — inventory that exists in **space and time**, not on a shelf.

---

## Table of Contents

- [Why an open-source transit reservation system?](#why-an-open-source-transit-reservation-system)
- [Key features](#key-features)
- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [API overview](#api-overview)
- [Bring your own providers](#bring-your-own-providers)
- [Architecture](#architecture)
- [Benchmarks](#benchmarks)
- [Project status & roadmap](#project-status--roadmap)
- [Use cases](#use-cases)
- [License](#license)

---

## Why an open-source transit reservation system?

Most transportation operators still run on legacy reservation software: expensive licenses, proprietary lock-in, monolithic deployments, and booking conflicts the moment demand spikes. General-purpose e-commerce platforms don't help — they assume inventory is static. Transit inventory isn't:

```
A ─── B ─── C ─── D        Seat 12A on one departure:

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
| **Multi-passenger itineraries** | One order, N passengers; per-passenger seats and fares, including regulated concession fares (child, senior, disability) that many markets mandate by law. |
| **Pluggable pricing (WASM)** | Deterministic integer-only fare rules, plus operator-supplied pricing modules run in a fuel-metered, memory-capped WebAssembly sandbox with **no host imports** — measured at p95 ≈ 443 µs per quote. Native and WASM engines are property-tested to be bit-identical. |
| **Signed quotes** | Short-TTL HMAC quote tokens: the price shown is the price charged, tamper-proof. |
| **Offline ticket validation** | Ed25519-signed CBOR QR tickets (~245 bytes). The `lulan-validate` crate verifies them with no server, no clock assumptions, and compiles to WebAssembly for browser and mobile boarding apps. |
| **Offline boarding sync** | Gate and crew devices journal scans locally and sync idempotent batches when connectivity returns; duplicate and cloned-QR scans are detected and flagged. |
| **Payments without lock-in** | The provider is a port described by configuration, not code: a JSON file names a PSP's create/refund/callback endpoints, auth style, field mappings and signature scheme. Stripe ships as a preset; anything else is a file. |
| **Headless & self-hostable** | JSON REST APIs only — bring your own storefront, kiosk, or POS. One Docker image, PostgreSQL + Redis, no per-booking fees. |

## How it works

A booking flows through seven stages — two optional, all independently verifiable:

1. **Search & availability** — `GET /v1/trips/search` returns candidate trips per leg (one-way or round-trip), each with operator, service number, vehicle, schedule, and span-aware seat/pool availability. `GET /v1/trips/{id}/availability` drills into the seat map, including which seats other sessions currently hold.
2. **Hold** *(optional)* — `POST /v1/holds` soft-holds the selected seats across every leg as **one itinerary hold** with a countdown (`expires_at`, operator-configurable). Expired holds auto-release with zero cleanup; buying with an expired hold is a deterministic 409. Holds never gate the sale — claims at order time are the source of truth.
   Holds are bounded so one session cannot take a fleet off sale: at most
   20 seats per request, and a configurable ceiling on how much of a trip
   may be held at once. Both are safe to be blunt about, because a refused
   hold is not a refused sale — the claim at order time is authoritative.
3. **Add-ons** *(optional)* — `GET /v1/ancillaries` lists everything the operator sells alongside the fare (baggage, meals, insurance, priority boarding), per-passenger or per-order, tied to one leg or the whole itinerary.
4. **Quote** — `POST /v1/quotes` prices the itinerary + add-ons (per passenger type, occupancy, peak day, round-trip and promo discounts) and returns a signed, short-lived quote token locking the full total.
5. **Order** — `POST /v1/orders` atomically claims every item on every leg for N passengers; a conflict on *any* leg rolls back everything. One order, one payment — fares and add-ons together.
6. **Pay & ticket** — a payment-provider webhook captures payment and auto-issues one Ed25519-signed QR ticket per passenger per leg.
7. **Board — even offline** — gate devices verify tickets locally against cached public keys (`GET /v1/ticket-keys`), then sync their scan journal (`POST /v1/scans`); the order aggregates to *Boarded* when the last passenger scans in.

## Quick start

Prerequisites: Rust (edition 2024), Docker, [`just`](https://github.com/casey/just).

```bash
git clone https://github.com/thinkgrid-labs/lulan.git
cd lulan
just up      # PostgreSQL 16 + Redis 7 (Docker)
just serve   # migrate, seed a sample multi-stop network, serve on :8080
```

Book a ticket end to end:

```bash
# 1. Find a departure (sample dataset: a 4-stop ferry line, BTG → … → CEB)
curl "localhost:8080/v1/trips/search?origin=BTG&destination=CEB&departure_date=$(date +%F)"

# 2. Quote a senior + child itinerary (concession fares applied automatically)
curl -X POST localhost:8080/v1/quotes -H 'content-type: application/json' -d '{
  "trip_id": "<trip>",
  "items": [
    {"unit_code": "12C", "origin": "BTG", "destination": "CEB", "passenger_type": "senior"},
    {"unit_code": "12D", "origin": "BTG", "destination": "CEB", "passenger_type": "child"}
  ]}'

# 3. Order with the quote token, pay (fake provider), fetch signed QR tickets.
#    Everything scoped to one order — paying it, cancelling it, reading its
#    tickets — needs that order's retrieval_token (returned at creation),
#    the owning customer's JWT, or an API key. The order id is not a
#    credential. Capture is server-to-server: integration key required.
curl -X POST localhost:8080/v1/orders ...
curl -X POST "localhost:8080/v1/orders/<id>/payment?token=<retrieval_token>"
curl -X POST localhost:8080/v1/payments/webhook \
  -H "x-api-key: $LULAN_BOOTSTRAP_ADMIN_KEY" -H 'content-type: application/json' \
  -d '{"payment_intent_id": "<intent>", "status": "succeeded"}'
curl "localhost:8080/v1/orders/<id>/tickets?token=<retrieval_token>"
```

Run the test suite and the concurrency harness:

```bash
just check                 # fmt + clippy + full test suite
just loadgen 10000 0.5     # 10k contenders, 50% via holds — expect 0 double-sells
just loadgen-paced 200 30  # open-loop 200 req/s — honest seat-lock latencies
```

## API overview

| Endpoint | Purpose |
|---|---|
| `GET /v1/trips/search` | One-way / round-trip search: candidate trips per leg with schedule + availability |
| `GET /v1/trips/{id}/availability` | Per-seat / per-pool availability for a journey span |
| `POST /v1/holds` | Soft-hold a one-way or round-trip seat selection as one itinerary hold (TTL, auto-release) |
| `GET /v1/ancillaries` | Add-on catalog: baggage, meals, insurance — whatever the operator sells |
| `POST /v1/quotes` | Itemised quote for fares + add-ons, locked by a signed token |
| `POST /v1/orders` | Atomic multi-passenger booking (live-priced or quote-token) |
| `POST /v1/orders/{id}/payment` | Create payment intent (provider port) — order credential required |
| `POST /v1/orders/{id}/cancel` | Cancel and release claims — order credential required |
| `POST /v1/payments/webhook` | Provider capture callback → auto-issues tickets (signature-authenticated, or integration key for unsigned providers) |
| `GET /v1/orders/{id}/tickets` | Ed25519-signed QR ticket tokens |
| `GET /v1/ticket-keys` | Public keys for offline validators |
| `POST /v1/scans` | Batched, idempotent boarding-scan sync (validator key) |
| `GET /v1/customers/me/orders` | Authenticated customer's bookings (IdP JWT) |
| `POST /v1/webhooks` | Register HMAC-signed webhook endpoints (admin key) |
| `GET /metrics` | Prometheus metrics |

The full surface is documented in the OpenAPI spec — committed at [`crates/lulan-api/openapi.json`](crates/lulan-api/openapi.json) and served live at `GET /openapi.json`. A typed TypeScript client ships as [`@lulan/storefront-sdk`](packages/storefront-sdk).

**Calling from a browser.** Lulan sends no CORS headers by default, so a
web storefront must either call through its own backend or have its origin
named in `LULAN_CORS_ALLOWED_ORIGINS` (comma-separated, or `*`). The API
returns passenger data and boarding-pass tokens; which sites may read
those is an operator decision, not a default.

**Selling stops when the departure does.** A trip id bypasses search, so
"still for sale?" is enforced where inventory resolves, not where it is
listed: quotes, holds, claims and orders all 409 once the trip has left
the requested origin (judged per leg — a service mid-journey can still
sell its later legs) or once ops has cancelled it. Availability stays
readable either way; crew and support still need to see past departures.

## Bring your own providers

Lulan never owns accounts and never touches card data — identity and
payments are **ports**: the core defines the contract, an adapter plugs
your provider in. (Reservation Sync Connectors, planned, follow the same
pattern for operational systems.)

### Identity provider — customers and staff

The core verifies a bearer JWT from *your* IdP and keeps only an
`(issuer, subject)` reference. No passwords, resets, sessions, or MFA in
Lulan — ever. One port serves both principals: a plain verified JWT is a
**customer**; a JWT that matches an enrolled `staff` row gains an
operator role (`admin` / `ops` / `support`).

```bash
# Ships today: HS256 shared-secret JWT (first-party storefront backends)
LULAN_IDP_ISSUER=https://auth.example.com
LULAN_IDP_HS256_SECRET=...            # your IdP's signing secret
LULAN_BOOTSTRAP_ADMIN_STAFF='https://auth.example.com|user-id-of-admin'
```

| Use case | How it maps |
|---|---|
| Ferry line with a Next.js storefront using **Supabase/Firebase auth** | Storefront session JWT goes straight to Lulan as the customer token — bookings attach to the customer, `GET /v1/customers/me/orders` lists them |
| Bus company on **Auth0 / Clerk / Keycloak** | Same trait, JWKS (RS256) adapter — planned; one adapter covers all JWKS-publishing IdPs |
| Walk-up / kiosk sales, no accounts at all | Skip the IdP entirely: **guest checkout** is first-class — `guest_contact` + an HMAC retrieval token (magic link) per order |
| Back-office staff signing into the admin app | Same IdP login; an admin enrols their identity via `POST /v1/admin/staff` with a role — every action they take is audited by name |

### Payment provider — configuration, not code

Most payment APIs are the same three shapes wearing different names: POST
somewhere to create an intent, POST somewhere to refund it, receive an
HMAC-signed callback. So a provider is **described**, not implemented — a
JSON file, no Rust, no rebuild, no fork. The same idea as pricing modules:
a runtime artifact the operator supplies.

```bash
# A built-in preset — this is the entire Stripe integration
LULAN_PAYMENT_PROVIDER=stripe
LULAN_PAYMENT_SECRET=sk_live_…
LULAN_PAYMENT_WEBHOOK_SECRET=whsec_…

# Anything else — describe it once
LULAN_PAYMENT_PROVIDER=/etc/lulan/my-psp.json
```

A description says where to POST, how to authenticate (bearer / basic /
custom header), whether bodies are JSON or form-encoded, which JSON
pointers hold the intent id and client secret, and how callbacks are
signed (SHA-256/512, hex/base64, raw or Stripe-style headers, replay
tolerance) — plus which of the provider's event names mean *captured* and
*failed*. Fully commented starter:
[`deploy/payment-providers/example.json`](deploy/payment-providers/example.json).
Stripe ships as a preset specifically to prove the description handles a
real, large PSP rather than a toy.

| Use case | How it maps |
|---|---|
| Global card payments (**Stripe**) | Built-in preset; supply two secrets. `payment_intent.succeeded` → order Paid → tickets auto-issue |
| PH e-wallets — GCash/Maya via **PayMongo or Xendit** | A description file: different URLs, JSON bodies, a raw signature header. No engine change |
| A bank gateway nobody has heard of | Same file. If its callbacks are unsigned, Lulan requires an integration API key on the webhook endpoint instead of trusting an open one |
| Something genuinely stranger (SOAP, an SDK, a redirect flow) | Implement the `PaymentProvider` trait in Rust — the escape hatch, not the expected path |
| Cash at a counter / agent network | A trusted `integration` API key confirms payment through the same webhook path — the state machine doesn't care who captured |
| Trip cancelled by ops, or support refunds a booking | Lulan calls `refund()` **before** releasing seats and voiding tickets — money moves first, inventory second |

Whatever the provider, the engine only ever sees *captured* or *failed*:
adapters translate, and an adapter cannot report an event it has not
authenticated. Verification and interpretation live in the same call on
purpose.

**With no provider configured** Lulan runs `FakeProvider`, which captures
payment without taking money. That is a demo, and it says so at boot.

Payment capture is idempotent (duplicate and out-of-order webhooks are
acknowledged, never re-applied), and `Idempotency-Key` on order creation
makes client retries double-booking-proof end to end: the key is claimed
*before* the order is written, so two concurrent retries cannot both book.
Keys are scoped to the caller and bound to the request body — one client's
key never replays another's order, and reusing a key for a different
request is refused rather than answered with an unrelated booking.

Money is recorded in the currency the fare ruleset priced it in, per
order. Lulan does not convert between currencies (multi-currency
settlement is out of scope for v1); an operator selling in several
publishes a ruleset per currency.

## Architecture

```
        Your storefront / kiosk / POS / boarding app
                          │
              JSON REST  ·  @lulan/storefront-sdk (TS)
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

   Offline edge:  lulan-validate (wasm32) verifies tickets
                  with zero server dependency.
```

## Benchmarks

Real numbers, adversarial shapes, published in [`docs/benchmarks.md`](docs/benchmarks.md):

- **0 double-sells** across 10,000 simultaneous contenders on one 52-seat vehicle — repeated with Redis killed mid-run (chaos test).
- **WASM pricing**: p50 347 µs / p95 443 µs per quote including per-call instantiation (PRD target < 5 ms), enforced by a CI assertion.
- **Ticket QR payload**: ~245 bytes signed (target < 400 bytes for low-error-correction QR).

## Project status & roadmap

> ⚠️ **Pre-1.0, active development.** APIs may change until the first stable release. Verified working today — the checked items below are implemented and covered by the test suite.

- [x] Segment-aware inventory engine (seats, pools, span claims)
- [x] Soft holds + race-free claims (0 double-sells @ 10k contenders)
- [x] Event-sourced order lifecycle with a configuration-driven payment-provider port (Stripe preset built in)
- [x] Pricing engine — native + sandboxed WASM modules, signed quotes
- [x] Multi-passenger orders with passenger-type fares
- [x] Ed25519 QR ticketing + offline validation (`lulan-validate`)
- [x] Offline boarding-scan sync with replay/clone detection
- [x] Webhooks: HMAC-signed deliveries with durable retries
- [x] Authentication: API keys + roles, identity-provider port, guest checkout with retrieval tokens
- [x] Order-scoped authorization — paying, cancelling, reading an order or its tickets each require that order's credential; the id alone is not one
- [x] Idempotent booking retries — the key is reserved before the write, scoped to the caller and bound to the request body — plus per-caller rate limiting
- [x] OpenAPI spec (served at `/openapi.json`) + TypeScript SDK (`@lulan/storefront-sdk`)
- [x] Prometheus `/metrics` (OTLP traces planned)
- [x] Itineraries: one-way, round-trip & multi-city (one atomic order across legs, round-trip fares)
- [x] Itinerary holds: one hold across all legs, TTL auto-release, live held-seat map
- [x] Ancillaries: operator add-on catalog (baggage, meals, insurance) priced into quotes & orders
- [x] Sale gating: departed and cancelled departures are refused wherever inventory resolves (a trip id bypasses search; sellability does not)
- [x] Per-currency orders — money is recorded in the currency the active fare ruleset priced it in
- [x] Hold stampede control: per-request seat cap + a configurable per-trip hold ceiling, neither of which can gate a sale
- [x] Opt-in CORS so a browser storefront can call the API directly
- [x] Production deploys: Compose (external or bundled databases, auto-TLS) + Helm chart
- [x] GTFS importer — bring your existing schedule feed
- [x] Open-loop benchmark mode (published seat-lock latencies vs the <20 ms target)
- [x] Admin operations API: staff RBAC (IdP-backed), network & schedule management, fare publishing with rollback, manifests, refunds — with `@lulan/admin-sdk`
- [ ] Reference Next.js storefront + React Native boarding-crew app
- [ ] `@lulan/validate` npm package (WASM build of the validator)
- [ ] Reservation Sync Connectors — first-class orchestrated mode (PSS / manifest / dispatch sync, external-ref mapping)

## Use cases

Lulan models any business that reserves **capacity over space and time**: regional and low-cost airlines, intercity and commuter bus lines, ferries and RoRo vessels, rail and metro networks, shuttle and van fleets, cargo and parcel space and vehicle-deck slots

**Standalone mode** fits operators without a sophisticated backend — provincial bus lines, ferry and tourism operators, shuttles, charters, small regional airlines: Lulan is the whole system, from search to boarding. **Orchestrated mode** fits enterprises with existing operational platforms: Lulan owns discovery → pricing → cart → payment → confirmed reservation, then a Reservation Sync Connector pushes it into the PSS / manifest system / dispatch backend (planned; today's HMAC-signed webhooks already enable the same integration DIY). Same API and domain model either way — only the connector changes.

## Contributing

Lulan is developed in the open and welcomes issues, design discussions, and pull requests. Contribution guidelines and architecture decision records will be published as the project approaches its first release.

## License

[Apache-2.0](LICENSE) — the whole repository: engine, API, pricing, SDKs,
the offline validator, deployment manifests and examples.

One licence, no carve-outs. Embed `lulan-validate` in a proprietary
boarding app, ship `@lulan/storefront-sdk` in a commercial storefront, run
a modified Lulan as a hosted service — all fine, and none of it needs a
lawyer's opinion first. The Apache patent grant travels with the code, and
its retaliation clause applies to anyone who sues over it.

---

**Keywords**: open-source reservation system · headless booking engine · reservation orchestration platform · airline reservation system · bus booking system · ferry reservation software · rail ticketing · seat reservation API · segment inventory · Rust booking engine · offline ticket validation · QR ticketing · WebAssembly pricing

*Lulan aims to be the open-source foundation for capacity reservation worldwide — bringing modern developer tooling to an industry still dominated by legacy software. The name comes from the Filipino word for "to board, to load." Transportation is only the beginning.*
