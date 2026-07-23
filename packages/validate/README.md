# @lulan/validate

Offline verification for Lulan QR boarding passes. The **exact** Rust code
that verifies a ticket on the server ([`lulan-validate`](../../crates/lulan-validate)),
compiled to WebAssembly — so a gate device, a kiosk, or a React Native
boarding app validates a ticket with **no server call**, no clock
assumptions, and no trust in the network.

## Install

```bash
npm install @lulan/validate
```

The package ships a `web`-target WASM build (browsers, bundlers, React
Native with WASM support). For Node, build the `nodejs` target with
`npm run build:node`.

## Use

```ts
import init, { verifyTicket } from "@lulan/validate";

// Browser / React Native: instantiate the module once at startup.
await init();

// Cache these two lists while online; validation is offline thereafter.
const keys = await fetch("/v1/ticket-keys").then(r => r.json()).then(r => r.keys);

try {
  const ticket = verifyTicket(
    scannedQrString,          // "LT1.…"
    keys,                     // [{ kid, public_key }]
    Date.now() / 1000,        // you own the clock
    boardingTripId,           // or null for an inspection scan
  );
  console.log(`Board ${ticket.passenger_name}, seat ${ticket.unit_code}`);
} catch (e) {
  // e.message is prefixed with a machine-readable code — branch on it.
  const code = e.message.split(":")[0]; // "expired" | "wrong_trip" | …
}
```

### Refusing refunded tickets offline

A signature proves a ticket was **issued**, never that it is still valid —
a refund happens after signing, so no offline check can derive it from the
ticket alone. Cache the operator's revocation list and pass it in:

```ts
import { verifyTicketWithRevocations } from "@lulan/validate";

const revoked = await fetch(`/v1/revocations?trip_id=${tripId}`)
  .then(r => r.json()).then(r => r.revoked); // string[]

const ticket = verifyTicketWithRevocations(qr, keys, Date.now() / 1000, tripId, revoked);
// throws "revoked: …" if the ticket was refunded or its trip cancelled
```

Coverage is bounded by how recently the device synced — a gate that has
never synced cannot know a ticket was pulled. That limit is honest and
unavoidable; it is the same one clone detection lives with.

## Error codes

`verifyTicket` throws an `Error` whose message is `"<code>: <detail>"`.
The codes: `malformed`, `unsupported_version`, `unknown_key`,
`bad_signature`, `expired`, `wrong_trip`, `revoked` (plus input-validation
codes `bad_key_set`, `bad_expected_trip`, `bad_revocations`).

## What a signature does and doesn't prove

- **Proves:** the ticket was issued by the operator and not altered.
- **Does not prove:** the QR wasn't *cloned* (a copy is a genuine copy —
  same-device re-scans are caught by a device-local seen-set; cross-device
  duplicates are detected server-side when scan journals sync), nor that
  it wasn't *revoked* (that is what the revocation list is for).

## Building from source

```bash
npm run build        # web target → pkg/
npm run build:node   # node target → pkg-node/
npm test             # builds the node target and runs the verification suite
```

The test runs against vectors emitted by the engine's own signer
(`crates/lulan-engine/examples/gen_ticket_vector.rs`), so the package is
proven against the exact bytes the server produces.

## License

Apache-2.0.
