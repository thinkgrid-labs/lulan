# Launch post outline: "Boarding passes that verify with the server off"

Audience: engineers who ship to the edge (transit, events, anywhere
connectivity is not a given). Goal: the offline-validation story as a
design study in honest threat modeling. ~1000 words.

## Spine

1. **Why this is hard where it matters (150w).** The places that most need
   ticketing — island ferries, provincial buses, festival gates — are
   exactly where the network isn't. A boarding pass that needs a server
   call is useless at the one moment it's scanned. Set the stakes.

2. **The shape (200w).** An Ed25519-signed CBOR payload, ~245 bytes, that
   fits a low-error-correction QR: ticket id, trip, seat/span, passenger
   hash, validity window, key id, signature. One pure verification crate
   (`lulan-validate`) — no clock (caller passes `now`), no network, no
   storage — compiled to WASM so the same code runs in the server, a
   browser, and a React Native gate. Show the `verifyTicket` call.

3. **The demo (150w).** The quickstart's step 8: book a ticket, cache the
   keys, *kill the server*, verify in Node. Paste the `VERIFIED OFFLINE`
   output. This is the whole pitch in eight lines.

4. **What a signature does NOT prove — the honest part (300w).** This is
   the section that earns trust. A signature proves *issued and
   unaltered*. It does not prove:
   - **Not cloned.** A copied QR is a genuine QR. Mitigation: device-local
     seen-set rejects same-device re-scans; cross-device duplicates are
     detected server-side when scan journals sync. We detect, we don't
     pretend to prevent — cross-device cloning across two offline gates is
     physics, not a crypto problem.
   - **Not revoked.** A refund happens *after* signing, so no offline
     check can derive it from the ticket. Mitigation: gates cache
     `GET /v1/revocations` and refuse pulled tickets. Coverage is bounded
     by how recently the device synced — a gate that never synced can't
     know. Say so plainly.
   The point: name the limits louder than the guarantees. "Replay
   detection, not replay prevention" is the honest promise.

5. **Operational reality (150w).** Keys rotate live (`/v1/admin/ticket-keys/rotate`),
   retired keys stay published so issued tickets keep verifying, and the
   private seed is encrypted at rest so a database dump can't forge passes.
   Losing the wrapping key costs only new signatures, never issued ones —
   a deliberate recoverability choice.

6. **Close (50w).** `@lulan/validate` on npm, Apache-2.0. Embed it in your
   own gate app. Link the package README + QUICKSTART.

## Assets
- The `verifyTicket` snippet + `VERIFIED OFFLINE` output.
- The threat-model table (proves / does not prove).
