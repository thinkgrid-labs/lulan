// Offline-verification smoke test against vectors produced by the engine's
// own signer (crates/lulan-engine/examples/gen_ticket_vector.rs), so the
// package is proven against the exact bytes the server emits — not a
// hand-rolled fixture that could drift from the wire format.
import { verifyTicket, verifyTicketWithRevocations } from "../pkg-node/lulan_validate.js";

const TOKEN =
  "LT1.qmF2AWN0aWRQERERERERQRGBEREREREREWN0cnBQIiIiIiIiQiKCIiIiIiIiImN1bnRjMTJBYWYAYXQDY3BheGlBbmEgUmV5ZXNiZmNnZWNvbm9teWNleHAa9IZXAGNraWRpd2lyZS10ZXN0.pFRMAeJjPNIXAI4cAFZiSodj7dib77ACk4puFRyoRsW3NLo-IWdGS-64_Fz1DTyPsTAW_QStUdBe0i8_vDV7Dw";
const KEYS = [{ kid: "wire-test", public_key: "GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE" }];
const TID = "11111111-1111-4111-8111-111111111111";
const TRP = "22222222-2222-4222-8222-222222222222";
const NOW = Math.floor(Date.parse("2024-01-01") / 1000);

let passed = 0;
const ok = (n, c) => { if (!c) { console.error("FAIL:", n); process.exit(1); } passed++; };
const throwsWith = (n, fn, code) => {
  try { fn(); console.error("FAIL (no throw):", n); process.exit(1); }
  catch (e) { ok(`${n} → ${code}`, e.message.startsWith(code)); }
};

const v = verifyTicket(TOKEN, KEYS, NOW, TRP);
ok("ticket_id", v.ticket_id === TID);
ok("trip_id", v.trip_id === TRP);
ok("unit_code", v.unit_code === "12A");
ok("passenger_name", v.passenger_name === "Ana Reyes");
ok("fare_class", v.fare_class === "economy");
ok("span", v.from_index === 0 && v.to_index === 3);
ok("inspection scan (no expected trip)", verifyTicket(TOKEN, KEYS, NOW, null).ticket_id === TID);

throwsWith("wrong trip", () => verifyTicket(TOKEN, KEYS, NOW, "33333333-3333-4333-8333-333333333333"), "wrong_trip");
throwsWith("expired", () => verifyTicket(TOKEN, KEYS, 4_200_000_000, TRP), "expired");
throwsWith("unknown key", () => verifyTicket(TOKEN, [{ kid: "other", public_key: KEYS[0].public_key }], NOW, TRP), "unknown_key");
throwsWith("tampered signature", () => verifyTicket(TOKEN.slice(0, -4) + "AAAA", KEYS, NOW, TRP), "bad_signature");
throwsWith("garbage token", () => verifyTicket("not-a-token", KEYS, NOW, TRP), "malformed");

ok("valid when revocation list omits it", verifyTicketWithRevocations(TOKEN, KEYS, NOW, TRP, []).ticket_id === TID);
throwsWith("revoked", () => verifyTicketWithRevocations(TOKEN, KEYS, NOW, TRP, [TID]), "revoked");

console.log(`@lulan/validate: ${passed} checks passed`);
