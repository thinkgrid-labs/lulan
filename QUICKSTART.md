# Lulan quickstart — book a ticket and validate it offline

Every command below was run against a fresh Lulan instance; the output is
real, not illustrative. From an empty database to a boarding pass verified
with the server switched off, in eight requests.

## 0. Bring it up

```bash
just up      # PostgreSQL 16 + Redis 7 (Docker)
just serve   # migrate, seed a sample inter-island ferry network, listen on :8080
```

`just serve` sets a dev `LULAN_QUOTE_SECRET` for you. It also runs with the
built-in **fake** payment provider, which captures payment without taking
money — a demo, not a deployment. In production you set
`LULAN_PAYMENT_PROVIDER=stripe` (or a description file) and real secrets.

The interactive API reference is served at **http://localhost:8080/docs**
(the raw spec is at `/openapi.json`).

The sample network is a 4-stop ferry line — Batangas (BTG) → Caticlan
(CTC) → Iloilo (ILO) → Cebu (CEB) — with 7 daily departures each way.

## 1. Search

A departure that has already left is refused at sale time, so book a
future date:

```bash
curl "localhost:8080/v1/trips/search?origin=BTG&destination=CEB&departure_date=2026-07-25"
```

```
Lulan Ferries · LUL 501 · economy free: 40 / 40
trip a065df73-0c78-4158-8934-75073b5fa0bf · departs 08:00 · 780 min
```

## 2. Quote a senior + a child

Concession fares are applied automatically from the passenger type — a
legal requirement in the PH market, so it is a fare input, not metadata.

```bash
curl -X POST localhost:8080/v1/quotes -H 'content-type: application/json' -d '{
  "trip_id": "a065df73-…",
  "items": [
    {"unit_code": "12C", "origin": "BTG", "destination": "CEB", "passenger_type": "senior"},
    {"unit_code": "12D", "origin": "BTG", "destination": "CEB", "passenger_type": "child"}
  ]}'
```

```json
{
  "currency": "PHP",
  "total_minor": 72000,
  "items": [
    { "unit_code": "12C", "passenger_type": "senior",
      "base_minor": 45000,
      "adjustments": [ {"label": "peak_weekday", "amount_minor": 6750},
                       {"label": "passenger:senior", "amount_minor": -9000} ],
      "total_minor": 42750 },
    { "unit_code": "12D", "passenger_type": "child",
      "base_minor": 45000,
      "adjustments": [ {"label": "peak_weekday", "amount_minor": 6750},
                       {"label": "passenger:child", "amount_minor": -22500} ],
      "total_minor": 29250 }
  ]
}
```

The response carries a signed `quote_token`; present it at order time to
buy at exactly these prices.

## 3. Order

Guest checkout (no account) with an `Idempotency-Key`, buying at the
quoted prices. The claim is atomic across every seat: a conflict on any
one rolls the whole order back.

```bash
curl -X POST localhost:8080/v1/orders -H 'content-type: application/json' \
  -H 'Idempotency-Key: demo-1' -d '{
  "trip_id": "a065df73-…",
  "passengers": [
    {"full_name": "Lola Remedios", "type": "senior"},
    {"full_name": "Anak Reyes", "type": "child"}
  ],
  "guest_contact": "family@example.com",
  "quote_token": "<from step 2>",
  "items": [
    {"unit_code": "12C", "origin": "BTG", "destination": "CEB", "passenger": 0},
    {"unit_code": "12D", "origin": "BTG", "destination": "CEB", "passenger": 1}
  ]}'
```

```
order_id 9d665256-9f0e-4474-b888-3186e528e78e · status locked · 72000 PHP
```

The response also returns a `retrieval_token` — the guest's credential for
everything scoped to this order (paying it, reading it, its tickets). The
order id alone is not a credential.

## 4. Pay

Create a payment intent. Gated by the order credential:

```bash
curl -X POST "localhost:8080/v1/orders/9d665256-…/payment?token=<retrieval_token>"
```

```
status pending_payment · provider fake · intent fake_pi_afe47e3ccba647ae…
```

## 5. Capture

The provider's callback. A real PSP authenticates this with a signature;
the fake provider does not, so it requires an integration API key. Capture
auto-issues one signed ticket per passenger.

```bash
curl -X POST localhost:8080/v1/payments/webhook \
  -H "x-api-key: $LULAN_BOOTSTRAP_ADMIN_KEY" -H 'content-type: application/json' \
  -d '{"payment_intent_id": "fake_pi_afe47e3c…", "status": "succeeded"}'
```

```json
{ "order_status": "ticketed", "applied": true }
```

## 6. Tickets

One Ed25519-signed QR per passenger (the `LT1.…` token renders to a QR):

```bash
curl "localhost:8080/v1/orders/9d665256-…/tickets?token=<retrieval_token>"
```

```
Lola Remedios · seat 12C · LT1.qmF2AWN0aWRQqRttFDYzT2uLCxuBJ5…
Anak Reyes    · seat 12D · LT1.qmF2AWN0aWRQ0Kom3LA0SgODDgKnvO…
```

## 7. Cache the key set

A gate device pulls the public keys once, while online:

```bash
curl localhost:8080/v1/ticket-keys
```

```
kid lulan-b0913a45 · pubkey s8qeHjl0FW9B06Hg7Acl1kR-…
```

## 8. Validate offline — the part that matters

Now **stop the server.** A boarding pass still has to verify at a gate
with no connectivity, and it does — the signature and the cached public
key are all `@lulan/validate` needs.

```js
import { verifyTicket } from "@lulan/validate";

const ticket = verifyTicket(
  scannedToken,        // "LT1.…" from the QR
  cachedKeys,          // [{ kid, public_key }] from step 7
  Date.now() / 1000,
  boardingTripId,      // or null for an inspection scan
);
```

```
VERIFIED OFFLINE — no server, no network:
  passenger: Lola Remedios
  seat:      12C (segments 0 → 3)
  trip:      a065df73-0c78-4158-8934-75073b5fa0bf
  signed by: lulan-b0913a45
```

A refund happens after signing, so a signature cannot prove a ticket is
still valid — cache `GET /v1/revocations` and use
`verifyTicketWithRevocations` to refuse pulled tickets offline too. See
[`@lulan/validate`](packages/validate/README.md).

---

That is the whole product: race-free inventory, per-passenger concession
fares, atomic multi-passenger orders, a payment port, signed QR tickets,
and boarding that survives zero connectivity — over plain JSON, no SDK
required.
