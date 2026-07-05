# Lulan

> **Open-source headless capacity reservation engine for modern transit.**

Lulan is an API-first reservation platform designed for transportation systems where inventory is both **spatial** (seats, cabins, vehicle slots) and **temporal** (available only across journey segments).

Unlike traditional e-commerce platforms that sell products, Lulan is built to manage **capacity**.

---

## Why Lulan?

Modern transportation operators still rely on legacy reservation systems that are:

- Expensive to license
- Difficult to customize
- Built around monolithic architectures
- Poorly suited for modern mobile experiences
- Prone to booking conflicts during peak demand
- Locked behind proprietary vendors

Meanwhile, general-purpose commerce platforms (Shopify, WooCommerce, Medusa, etc.) assume inventory is static.

Transportation inventory isn't.

A single seat may be occupied for one segment of a trip and become available for another.

```
A ─── B ─── C ─── D

Seat 12A

A → B  Reserved
B → C  Available
C → D  Reserved
```

Managing inventory across both **space** and **time** requires a different kind of reservation engine.

That's what Lulan provides.

---

# What Lulan Solves

Lulan is designed for systems that reserve **capacity**, including:

- 🚌 Provincial buses
- ⛴️ Ferries
- ✈️ Regional airlines
- 🚆 Rail operators
- 🚐 Shuttle services
- 🚗 Parking systems
- 🎟️ Timed reservations
- 📦 Cargo & logistics

Core capabilities include:

- Segment-aware inventory
- High-concurrency seat locking
- Event-driven reservation lifecycle
- Dynamic pricing
- Offline ticket validation
- Headless APIs
- Self-hostable infrastructure

---

# Principles

Lulan is built around a few core ideas.

## API First

The core never renders HTML.

Everything is exposed through stable, documented APIs.

Build your own website, mobile app, kiosk, or POS.

---

## Capacity First

Everything is reservable.

- Seats
- Cabins
- Vehicle spaces
- Cargo
- Baggage
- Meals
- Priority boarding

---

## Built for Concurrency

A physical seat is a shared resource.

Lulan is designed to safely coordinate thousands of simultaneous reservation attempts without overselling inventory.

---

## Local First

Ground staff should continue operating even without internet connectivity.

Offline validation is a first-class feature.

---

## Open by Default

The core is fully open source and self-hostable.

No vendor lock-in.

No per-booking transaction fees.

---

# Planned Architecture

```
                +----------------------+
                |  Next.js Storefront  |
                +----------+-----------+
                           |
                +----------v-----------+
                |  TypeScript SDK      |
                +----------+-----------+
                           |
                +----------v-----------+
                |     Lulan Core       |
                |   Rust / Axum API    |
                +----------+-----------+
                           |
      +---------+----------+-----------+---------+
      |         |                      |         |
 PostgreSQL   Redis             Kafka/Redpanda  Wasm
```

---

# Project Status

> **⚠️ Active Development**

Lulan is currently in its early development phase.

The public API, architecture, and repository structure are expected to evolve rapidly until the first stable release.

Breaking changes should be expected.

---

# Roadmap

- [ ] Reservation engine
- [ ] Segment-aware inventory
- [ ] Event sourcing
- [ ] TypeScript SDK
- [ ] Authentication
- [ ] Pricing engine
- [ ] Offline validation
- [ ] Reference storefront
- [ ] React Native conductor app

---

# Open Source

Lulan is developed in the open.

Contributions, discussions, design proposals, and feedback are welcome.

Documentation and contribution guidelines will be published as the project matures.

---

# License

Lulan uses a dual-license model.

| Package | License |
|----------|---------|
| Core Engine | AGPL-3.0 |
| SDKs | MIT |
| UI Components | MIT |
| Examples | MIT |

See the `LICENSE` files in each package for details.

---

# Vision

Lulan aims to become the open-source foundation for capacity reservation systems, bringing modern developer tooling to an industry still dominated by legacy software.

Transportation is only the beginning.
