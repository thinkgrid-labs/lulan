# @lulan/storefront-sdk

Typed, zero-dependency TypeScript client for the [Lulan](https://github.com/thinkgrid-labs/lulan) reservation engine. Apache-2.0 — use it in commercial storefronts freely.

Works in Node 18+, browsers, and edge runtimes (anything with `fetch`).

## Install

```bash
npm install @lulan/storefront-sdk
```

## The whole booking flow

```ts
import { LulanClient } from "@lulan/storefront-sdk";

const lulan = new LulanClient({ baseUrl: "https://api.operator.example" });

// 1. Search
const { trips } = await lulan.searchTrips({
  origin: "BTG", destination: "CEB", date: "2026-08-01",
});

// 2. Quote (passenger types drive mandated discounts)
const quote = await lulan.createQuote({
  trip_id: trips[0].trip_id,
  items: [
    { unit_code: "12C", origin: "BTG", destination: "CEB", passenger_type: "senior" },
    { unit_code: "12D", origin: "BTG", destination: "CEB", passenger_type: "child" },
  ],
});

// 3. Book at the quoted price — idempotency key makes retries safe
const order = await lulan.createOrder(
  {
    trip_id: trips[0].trip_id,
    passengers: [
      { full_name: "Remedios Cruz", type: "senior", birthdate: "1952-06-12" },
      { full_name: "Paolo Cruz", type: "child" },
    ],
    guest_contact: "remedios@example.com",
    quote_token: quote.quote_token,
    items: [
      { unit_code: "12C", origin: "BTG", destination: "CEB", passenger: 0 },
      { unit_code: "12D", origin: "BTG", destination: "CEB", passenger: 1 },
    ],
  },
  { idempotencyKey: crypto.randomUUID() },
);
// Keep order.retrieval_token — it is the guest's read credential.

// 4. Pay (provider port; FakeProvider in dev)
await lulan.requestPayment(order.order_id);

// 5. Tickets: one signed QR token per passenger
const { tickets } = await lulan.getTickets(order.order_id, order.retrieval_token);
```

## Authenticated customers

If the operator configures an IdP, pass the signed-in user's JWT — orders
attach to the customer and `guest_contact` becomes optional:

```ts
lulan.setCustomerToken(sessionJwt);
const order = await lulan.createOrder({ ... });      // customer-owned
const mine = await lulan.myOrders();                  // list their bookings
await lulan.claimOrder(guestOrderId, retrievalToken); // adopt a guest order
```

## Server-to-server

```ts
const backend = new LulanClient({
  baseUrl: "https://api.operator.example",
  apiKey: process.env.LULAN_API_KEY, // integration or validator role
});
await backend.syncScans("gate-1", journal); // crew devices
```

## Errors

Every non-2xx response throws `LulanApiError` with `status` and the parsed
body — `409` on inventory races, `429` on rate limits, `401`/`403` on
credential problems.

The API surface is documented in the engine's OpenAPI spec, served at
`GET /openapi.json`.
