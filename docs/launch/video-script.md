# Walkthrough video script (~3 min)

The reproducible artifact is this script; the recording is a follow-up.
Every command matches [QUICKSTART.md](../../QUICKSTART.md), so the video is
just the quickstart, narrated, in one unbroken terminal + browser take.

Setup before recording: `just up && just serve` already running; a second
terminal ready; a browser tab on `localhost:8080/docs`.

---

**[0:00] The hook — cold open on the last step, then rewind.**
Terminal shows `verifyTicket(...)` printing `VERIFIED OFFLINE — no server,
no network`. Narration: "This ferry ticket just verified at a boarding
gate with the server turned off. Here's the whole system that gets you
there, in three minutes."

**[0:15] The problem (10s, on the segment diagram from the README).**
"Transit inventory isn't a shelf. The same seat is sold on one leg and
free on the next. Sell it wrong under load and you double-book a
passenger. That's the problem Lulan is built for."

**[0:25] Search.** Run step 1. "One ferry line, seven departures. Forty
economy seats, all free."

**[0:40] Quote.** Run step 2. Point at the two adjustment lines. "Senior
and child fares aren't a discount code — they're computed from the
passenger type, because in this market they're the law. Peak surcharge,
concession, done. The total is signed into a token."

**[1:05] Order.** Run step 3. "Two passengers, guest checkout, no account.
The claim is atomic — if either seat lost a race, the whole order rolls
back. Nobody half-books." Note the `retrieval_token`: "that's the guest's
only credential; the order id gets you nothing."

**[1:30] Pay + capture.** Run steps 4-5. "A payment intent, then the
provider's callback captures it and auto-issues the tickets. The provider
is a config file, not a code fork — Stripe's a preset."

**[1:55] Tickets + keys.** Run steps 6-7. "One Ed25519-signed QR per
passenger. A gate pulls the public keys once, while it still has signal."

**[2:15] The payoff — kill the server.** `Ctrl-C` the `just serve`
terminal, visibly. Run step 8. "Server's down. The gate still boards the
passenger, because the signature and the cached key are all it needs. No
connectivity, no problem — that's the whole point of an island ferry, a
rural bus, a festival gate."

**[2:45] Close, on `localhost:8080/docs`.** "Race-free inventory,
concession fares, atomic orders, offline boarding — plain JSON, one Docker
image, Apache-2.0. Try it: the full transcript is in QUICKSTART, the API's
right here at slash-docs."

---

Shot notes: keep the terminal font large; the two beats that land are the
concession-fare adjustments (people don't expect fares to be *computed*)
and the `Ctrl-C` before offline validation (make the kill obvious).
