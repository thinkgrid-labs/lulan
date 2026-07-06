# Benchmarks

## Holds & claims — zero double-sells under adversarial load

Honest numbers from the invariant harness (`lulan-loadgen`), per the plan's
"publish real numbers" policy. These are **worst-case adversarial bursts**,
not steady-state throughput: every contender fires simultaneously (barrier
start) at one 52-seat vessel — a shape crueler than any real holiday spike.

### Setup

- MacBook (Apple silicon), macOS 15 — server, Postgres 16 (Docker), Redis 7
  (Docker), and loadgen all on one machine sharing cores
- `lulan-api` release build, 50-connection PG pool, HTTP/2
- 10,000 contenders, 52 seats, 6 span variants, uniform seat targeting

### Results (2026-07-05)

| Run | Contenders | Holds | Outcome |
|---|---|---|---|
| A: claim-only | 10,000 | 0% | **0 double-sells**, 112 claimed / 9,888 conflicts / 0 errors, 14.1s wall |
| B/C: chaos — Redis killed at t+5s | 10,000 | 50% | **0 double-sells**, 110 claimed / 9,832 conflicts / 58 client timeouts, 61.5s wall |

Latency under the 10k burst (µs, includes queueing — see caveats):
claim p50 ≈ 7.3–8.6M, p95 ≈ 12.3–13.4M; hold p50 ≈ 6.5M.

### What this does and doesn't show

- **Shows:** the zero-double-sell invariant holds at 10k simultaneous
  contenders, including with Redis killed mid-run. Winners' spans are
  pairwise disjoint per seat and the final DB masks equal exactly the
  winners' union. Timed-out clients never resulted in silent sells.
- **Doesn't show:** per-request service latency. The p50/p95 above are
  dominated by queue depth (10k requests, one burst, shared-core setup).
  The PRD's <20 ms seat-lock target must be measured at realistic arrival
  rates on separated hardware — planned for Phase 7 alongside a paced
  (open-loop) load mode in lulan-loadgen.
- 58 errors in the chaos run are client-side 60s timeouts in the tail of
  the burst, not server failures (server log shows none).

Reproduce: `just up && just serve`, then `just loadgen 10000 0.5`.

## Seat-lock latency — paced open-loop (2026-07-06)

The PRD's <20 ms seat-lock target, measured honestly: open-loop arrivals
(requests fire on a fixed clock and never wait for earlier responses), so
these are true per-request service latencies — not queue depth. Release
build, same shared-core laptop setup as above; latencies are full HTTP
round trips against the 52-seat vessel, conflicts included (a denial
exercises the same guarded UPDATE as a win).

| Load shape | p50 | p95 | p99 | PRD <20 ms (p95) |
|---|---|---|---|---|
| 200/s × 30 s, claims only | 6.6 ms | 9.9 ms | 31 ms | **PASS** |
| 200/s × 30 s, 50% via holds (claim) | 8.7 ms | 13.0 ms | 37 ms | **PASS** |
| 200/s × 30 s, 50% via holds (hold) | 6.8 ms | 10.3 ms | 69 ms | — |
| 500/s × 20 s, claims only | 21.0 ms | 38.6 ms | 61 ms | MISS |

Zero double-sells and zero transport errors in every run. The 500/s miss
is published as-is: it's 2.5× the PRD's reference rate on a laptop
sharing cores with Postgres, Redis, Docker, and the harness itself —
separated production hardware is expected to clear it, but we won't claim
that until measured.

Reproduce: `just serve-release`, then `just loadgen-paced 200 30`.

## Pricing — sandboxed WASM modules

WASM pricing module (wasmtime host, per-call instantiation, fuel-metered,
same machine as above):

| Metric | Measured | PRD target |
|---|---|---|
| price() p50 | 347 µs | <5 ms |
| price() p95 | 443 µs | <5 ms |

Measured by `wasm_call_latency_within_prd_target` in
`crates/lulan-pricing/tests/differential.rs` (200 samples after warm-up);
the assertion enforces the 5 ms bound in CI. Native engine evaluation is
sub-microsecond. Differential testing (64 proptest cases per run) holds
native and WASM engines to bit-identical quotes and errors.
