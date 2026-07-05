# Product Requirements Document (PRD)

**Project Name:** Lulan
**Tagline:** Open-source Headless Capacity Reservation Engine for Modern Transit
**Document Status:** Draft v2

---

## 1. Executive Summary

Modern transportation operators—from regional airlines and inter-island ferries to provincial bus companies—depend on reservation systems that were built decades ago. These systems are expensive, difficult to customize, tightly coupled to outdated user interfaces, and often fail under peak booking demand.

General-purpose e-commerce platforms solve product sales, but transportation does not sell products—it sells **capacity**.

Capacity is both:

- **Spatial** (a physical seat, cabin, vehicle slot, or cargo space)
- **Temporal** (available only across specific journey segments)

Lulan Core is an open-source, API-first reservation engine designed specifically for capacity-based businesses.

It provides:

- Segment-aware inventory management
- High-concurrency seat locking
- Event-driven order processing
- Edge-executed pricing
- Offline ticket validation
- Modern developer tooling

Unlike legacy Passenger Service Systems (PSS), Lulan Core allows operators to own their infrastructure, build custom customer experiences, and avoid vendor lock-in or transaction fees.

## 2. Problem Statement

Current reservation systems suffer from several industry-wide limitations.

### Legacy Passenger Service Systems

- Expensive licensing
- Vendor lock-in
- Limited API support
- Slow customization
- Monolithic architecture

### Generic E-commerce Platforms

Platforms like Shopify or WooCommerce assume inventory is static. Transportation inventory is dynamic.

Example — Seat 12A:

| Segment | Status |
|---|---|
| A → B | Occupied |
| B → C | Available |
| C → D | Occupied |

Traditional commerce systems cannot model inventory across journey segments without extensive custom development.

### Operational Challenges

Operators commonly experience:

- Overbooking during holiday demand
- Race conditions during checkout
- Failed payment reconciliation
- Poor offline support
- Separate systems for booking, ticketing, and validation
- Slow mobile experiences

## 3. Vision

To become the open-source infrastructure standard for capacity reservation systems.

Just as Stripe became payment infrastructure and Medusa became headless commerce infrastructure, Lulan aims to become the reservation engine powering modern transportation platforms.

## 4. Product Principles

### Headless by Default

The core exposes APIs only. No HTML. No templates. No opinionated frontend.

### Capacity First

Everything is inventory: a seat, a cabin, a vehicle slot, cargo space, priority boarding, meals, ancillary services. All are represented as reservable capacity.

### Local First

Critical workflows continue operating without connectivity: ticket validation, boarding, passenger lookup, cached schedules.

### Edge Native

Business rules execute close to users. Pricing logic, discounts, feature flags, and fare calculation run as WebAssembly modules.

### Extreme Concurrency

The system guarantees that the same inventory cannot be sold twice, even under massive simultaneous demand.

### Developer Experience First

Every public API is documented, typed, versioned, and designed for SDK generation.

## 5. Target Users

### Software Integrators

Need: strong typing, SDKs, extensibility, stable APIs.
Success metric: deploy custom reservation systems rapidly.

### Transit Operators

Examples: provincial bus companies, ferry operators, regional airlines.
Need: reliability, high availability, lower operating costs.
Success metric: no oversold seats during peak booking.

### Ground Staff

Need: fast scanning, offline validation, rugged mobile workflows.
Success metric: continue boarding passengers during network outages.

## 6. Customer Journey

Search Trip → Select Segment → Lock Inventory → Calculate Dynamic Price → Checkout → Payment → Ticket Issued → QR Generated → Boarding Validation → Trip Completed

Every transition produces immutable domain events.

## 7. Core Features

### 7.1 Capacity & Inventory Engine

Supports: airline seating, ferry cabins, vehicle decks, cargo, parking slots, bus seating.

Capabilities: spatial inventory, temporal inventory, segment availability, weight constraints, capacity rules.

### 7.2 High-Concurrency Reservation

Guarantees: atomic seat locking, reservation expiration, distributed lock coordination, zero duplicate reservations.

Designed to withstand holiday booking spikes.

### 7.3 Event-Driven Order Engine

Order lifecycle:

Draft → Locked → Pending Payment → Paid → Ticketed → Boarded → Completed → Refunded (optional)

Every transition creates immutable events, e.g. `SeatLocked`, `PaymentAuthorized`, `PaymentCaptured`, `TicketIssued`, `PassengerBoarded`.

### 7.4 Dynamic Pricing

Supports: fare classes, promotions, occupancy pricing, peak-hour pricing, ancillary pricing.

Rules execute via WebAssembly. Benefits: instant pricing updates, lower API latency, reduced backend load.

### 7.5 Offline Validation

Ground staff can scan QR codes, verify signatures, validate tickets, and sync later. No permanent internet connection required.

## 8. API Philosophy

Lulan Core exposes:

- REST APIs for public clients
- Event streams for internal services
- Webhooks for integrations
- OpenAPI specifications
- Generated TypeScript SDKs

Future consideration: GraphQL gateway, gRPC internal APIs.

## 9. Extensibility

Operators can customize through: plugins, event subscribers, pricing modules, custom validators, fare engines, webhooks.

The core remains stable while business rules remain replaceable.

## 10. Security

- JWT authentication
- OAuth2 support
- Role-based access control
- Signed QR tickets
- Audit logging
- Replay attack prevention
- Immutable event history

## 11. Observability

Built-in support for: OpenTelemetry, Prometheus metrics, structured logging, distributed tracing, health endpoints.

## 12. Deployment

### Small Operators

Docker Compose, single PostgreSQL, single Redis.

### Growing Operators

Docker Swarm, Nomad, multiple worker nodes.

### Enterprise

Kubernetes, high availability, redundant PostgreSQL, Kafka/Redpanda clusters, multi-region deployments.

## 13. Architecture

| Component | Technology | Purpose |
|---|---|---|
| Core API | Rust + Axum | Reservation engine |
| Async Runtime | Tokio | High concurrency |
| Database | PostgreSQL + SQLx | Read models |
| Event Store | Kafka / Redpanda | Immutable events |
| Cache | Redis | Distributed locking |
| Edge Runtime | WebAssembly | Pricing & validation |
| SDK | TypeScript | Client integrations |

## 14. Performance Goals

Target metrics for Version 1:

| Metric | Target |
|---|---|
| Seat lock latency | <20 ms |
| API response | <100 ms (p95) |
| Concurrent seat locks | 50,000+ |
| Booking throughput | 500+ bookings/sec |
| QR validation | <100 ms offline |
| Pricing execution | <5 ms (edge) |

(Targets should be validated through benchmarking during implementation.)

## 15. Migration Strategy

Recognizing operators rarely replace systems overnight, Lulan supports gradual adoption.

Migration tools include: CSV importers, schedule import, passenger migration, legacy reservation adapters, event replay, dual-write integrations.

This enables phased migration instead of "big bang" replacement.

## 16. Developer Experience

### @lulan/storefront-sdk

Strict TypeScript SDK, generated directly from OpenAPI.

### Shared UI Components

Mobile-first components: seat maps, schedule search, passenger forms, cart, ticket viewer, QR scanner.

### Reference Applications

- Next.js storefront
- React Native conductor app
- Admin dashboard
- API playground

## 17. Competitive Positioning

| Capability | Generic E-commerce | Legacy PSS | Lulan Core |
|---|---|---|---|
| Segment inventory | ❌ | ✅ | ✅ |
| Headless API | Partial | Limited | ✅ |
| Event sourcing | ❌ | Rare | ✅ |
| Offline validation | ❌ | Partial | ✅ |
| Dynamic edge pricing | ❌ | ❌ | ✅ |
| Open source | Rare | ❌ | ✅ |
| Self-hostable | Partial | ❌ | ✅ |

## 18. Open Source Strategy

### Licensing

- **Core Engine:** AGPL v3 — encourages improvements to remain open while protecting the community from closed SaaS forks.
- **SDKs, UI Libraries & Examples:** MIT — allows organizations to build proprietary applications on top of Lulan without friction.

## 19. Future Commercial Model

Lulan will remain fully open source. Future sustainability may include: managed cloud hosting, enterprise support, high-availability deployments, migration consulting, premium observability, managed event infrastructure.

## 20. Long-Term Vision

While Lulan initially targets transportation, its underlying reservation model applies to any business that manages capacity over time and space.

Potential future verticals: rail, event venues, parking, campgrounds, marinas, co-working spaces, equipment rentals, time-slot reservations.

The long-term goal is for Lulan to become the open standard for capacity reservation infrastructure, with transportation serving as the first and most demanding domain.
