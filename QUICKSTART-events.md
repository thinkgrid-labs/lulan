# Lulan quickstart — sell a concert with the ferry engine

This is the [ferry quickstart](QUICKSTART.md) run against a **concert**, to
show the engine is domain-agnostic. Same eight requests, same API — only
the dataset changes. Sections become fare-class seats, general admission
becomes a pool, and doors → end is a single segment. The output below is
real, not illustrative.

> A Lulan deployment serves **one domain**: the active fare ruleset is
> global. `just serve-events` seeds the concert profile into its **own**
> `lulan_events` database and never touches the ferry `lulan` one. "The
> ferry engine" means the same code and API — not the same instance.

## 0. Bring it up

```bash
just up             # PostgreSQL 16 + Redis 7 (Docker)
just serve-events   # create lulan_events, seed the arena show, listen on :8080
```

The sample dataset is a live-music venue — **The Big Dome**, operated by
**Sirena Live** — running three nights (`NIGHT 1/2/3`). The capacity, per
night, is 100 reserved seats across three sections (VIP 20, lower box 40,
upper box 40) plus **5,000 general admission**. Prices are in **USD**: VIP
$250, lower box $120, upper box $75, GA $45.

`just serve-events` sets a dev `LULAN_QUOTE_SECRET` and a bootstrap
integration key (`llk_events_admin_key`), and runs the built-in **fake**
payment provider — it captures payment without taking money. A demo, not a
deployment.

## 1. Search — the show is a "departure"

Doors (`DOORS`) → end of night (`END`) on the first show night:

```bash
curl "localhost:8080/v1/trips/search?origin=DOORS&destination=END&departure_date=<night-1-date>"
```

```
Sirena Live · NIGHT 1
sections   vip 20/20 · lower_box 40/40 · upper_box 40/40
GA pool    GENERAL_ADMISSION remaining 5000
trip 796cfac7-d221-451d-8365-8f33f20d1831 · 1 segment
```

## 2. Quote — a VIP seat + two general admission

```bash
curl -X POST localhost:8080/v1/quotes -H 'content-type: application/json' -d '{
  "trip_id": "796cfac7-…",
  "items": [
    {"unit_code": "VIP-1", "origin": "DOORS", "destination": "END", "passenger_type": "adult"},
    {"unit_code": "GENERAL_ADMISSION", "origin": "DOORS", "destination": "END",
     "passenger_type": "adult", "quantity": 2}
  ]}'
```

```json
{
  "currency": "USD",
  "total_minor": 37400,
  "items": [
    { "unit_code": "VIP-1", "quantity": 1, "base_minor": 25000,
      "adjustments": [ {"label": "peak_weekday", "amount_minor": 2500} ],
      "total_minor": 27500 },
    { "unit_code": "GENERAL_ADMISSION", "quantity": 2, "base_minor": 9000,
      "adjustments": [ {"label": "peak_weekday", "amount_minor": 900} ],
      "total_minor": 9900 }
  ]
}
```

$374.00 — a $275 VIP seat and two $49.50 GA (a weekend-night surcharge on
both). The response carries a signed `quote_token`; present it at order
time to buy at exactly these prices.

## 3. Order — one named buyer, three admissions

The VIP seat is reserved for a named holder (`passenger` index); the two
GA are order-level and bearer. One buyer is enough.

```bash
curl -X POST localhost:8080/v1/orders -H 'content-type: application/json' \
  -H 'Idempotency-Key: show-1' -d '{
  "trip_id": "796cfac7-…",
  "passengers": [{"full_name": "Maya Cruz", "type": "adult"}],
  "guest_contact": "maya@example.com",
  "quote_token": "<from step 2>",
  "items": [
    {"unit_code": "VIP-1", "origin": "DOORS", "destination": "END",
     "passenger": 0, "passenger_type": "adult"},
    {"unit_code": "GENERAL_ADMISSION", "origin": "DOORS", "destination": "END",
     "passenger_type": "adult", "quantity": 2}
  ]}'
```

```
order_id 3fe57015-… · status locked · 37400 USD
```

Keep the `retrieval_token` from the response — it is the guest's credential
for paying, reading, and ticketing this order. The id alone is not one.

## 4. Pay

```bash
curl -X POST "localhost:8080/v1/orders/3fe57015-…/payment?token=<retrieval_token>"
```

```
status pending_payment · provider fake · intent fake_pi_eef630c5…
```

## 5. Capture

The provider's callback. The fake provider is unauthenticated, so it
requires an integration API key. Capture auto-issues the tickets.

```bash
curl -X POST localhost:8080/v1/payments/webhook \
  -H "x-api-key: llk_events_admin_key" -H 'content-type: application/json' \
  -d '{"payment_intent_id": "fake_pi_eef630c5…", "status": "succeeded"}'
```

```json
{ "order_status": "ticketed", "applied": true }
```

## 6. Tickets — one signed QR per admission

Three admissions, three QRs: the VIP seat keeps Maya's name; each GA unit
is its own bearer pass (holder label "General Admission").

```bash
curl "localhost:8080/v1/orders/3fe57015-…/tickets?token=<retrieval_token>"
```

```
General Admission · GENERAL_ADMISSION · LT1.qmF2AWN0aWRQ…
General Admission · GENERAL_ADMISSION · LT1.qmF2AWN0aWRQ…
Maya Cruz         · VIP-1             · LT1.qmF2AWN0aWRQ…
```

General admission is an **admission** pool: one boarding pass per unit
claimed. A bulk pool — cargo kilograms, a vehicle deck — issues none; the
quantity there is weight or vehicles, not people.

## 7. Validate offline — at the gate, server switched off

Identical to the ferry flow: cache the key set once (`GET
/v1/ticket-keys`), then verify with [`@lulan/validate`](packages/validate)
and no network. A bearer GA QR:

```js
import { verifyTicket } from "@lulan/validate";

const t = verifyTicket(gaToken, cachedKeys, Date.now() / 1000, showTripId);
// { pax: "General Admission", unt: "GENERAL_ADMISSION", trp: "796cfac7-…", fc: null }
```

```
GA QR VERIFIED OFFLINE at the gate — no server, no network:
  holder:   General Admission
  section:  GENERAL_ADMISSION
  show:     796cfac7-d221-451d-8365-8f33f20d1831
  fare_cls: (bearer, none)
```

Same engine, same signatures, same offline guarantee — a concert instead
of a crossing.
