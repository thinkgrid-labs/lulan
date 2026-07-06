//! Load harness: the zero-double-sell invariant checker (Phase 2) plus a
//! paced open-loop mode (Phase 7) for honest latency numbers.
//!
//! Modes (`MODE` env):
//! - `burst` (default): every contender fires simultaneously at one
//!   vessel — the adversarial worst case. Latencies include queueing and
//!   are NOT service latencies; the point is the invariant.
//! - `paced`: open-loop arrivals at `RATE` requests/second for
//!   `DURATION_SECS` — the realistic shape. Arrivals never wait for
//!   earlier responses (open loop), so reported latencies are true
//!   per-request service latencies, comparable to the PRD's <20 ms
//!   seat-lock target.
//!
//! Environment:
//! - `DATABASE_URL`   (required) — pick the target trip, reset occupancy,
//!   verify the invariant afterwards.
//! - `LULAN_URL`      (default `http://127.0.0.1:8080`)
//! - `MODE`           (`burst` | `paced`, default `burst`)
//! - `CONTENDERS`     (burst; default `10000`)
//! - `RATE`           (paced; arrivals/second, default `200`)
//! - `DURATION_SECS`  (paced; default `30`)
//! - `HOLD_RATIO`     (default `0.0`) — fraction acquiring a soft hold
//!   first (exercises Redis; hold failures tolerated for chaos runs).
//!
//! Exit code is non-zero if any invariant is violated.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use rand::Rng;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[derive(Clone)]
struct Task {
    seat: String,
    origin: &'static str,
    destination: &'static str,
    from: u8,
    to: u8,
    use_hold: bool,
}

#[derive(Debug)]
struct Outcome {
    seat: String,
    mask: u64,
    claimed: bool,
    error: bool,
    hold_error: bool,
    claim_us: u128,
    hold_us: Option<u128>,
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p / 100.0).round() as usize;
    sorted[idx]
}

const CODES: [&str; 4] = ["BTG", "CTC", "ILO", "CEB"];
const SPANS: [(u8, u8); 6] = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];

async fn run_task(client: reqwest::Client, base: String, trip_id: Uuid, task: Task) -> Outcome {
    let mut hold_us = None;
    let mut hold_id: Option<String> = None;
    let mut hold_error = false;
    if task.use_hold {
        let t0 = Instant::now();
        let result = client
            .post(format!("{base}/v1/holds"))
            .json(&serde_json::json!({
                "trip_id": trip_id,
                "items": [{
                    "unit_code": task.seat,
                    "origin": task.origin,
                    "destination": task.destination,
                }],
            }))
            .send()
            .await;
        hold_us = Some(t0.elapsed().as_micros());
        match result {
            Ok(resp) if resp.status().as_u16() == 201 => {
                hold_id = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["hold_id"].as_str().map(String::from));
            }
            Ok(_) => {}
            Err(_) => hold_error = true,
        }
    }

    let t0 = Instant::now();
    let result = client
        .post(format!("{base}/v1/trips/{trip_id}/claims"))
        .json(&serde_json::json!({
            "unit_code": task.seat,
            "origin": task.origin,
            "destination": task.destination,
            "hold_id": hold_id,
        }))
        .send()
        .await;
    let claim_us = t0.elapsed().as_micros();

    let width = task.to - task.from;
    let mask = ((1u64 << width) - 1) << task.from;
    match result {
        Ok(resp) => Outcome {
            seat: task.seat,
            mask,
            claimed: resp.status().as_u16() == 201,
            error: !matches!(resp.status().as_u16(), 201 | 409),
            hold_error,
            claim_us,
            hold_us,
        },
        Err(_) => Outcome {
            seat: task.seat,
            mask,
            claimed: false,
            error: true,
            hold_error,
            claim_us,
            hold_us,
        },
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<ExitCode> {
    let base_url =
        std::env::var("LULAN_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let mode = std::env::var("MODE").unwrap_or_else(|_| "burst".to_string());
    let contenders: usize = std::env::var("CONTENDERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let rate: u64 = std::env::var("RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let duration_secs: u64 = std::env::var("DURATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let hold_ratio: f64 = std::env::var("HOLD_RATIO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    // Target: the latest OUTBOUND trip (the harness's spans assume the
    // BTG→…→CEB stop order); reset all its seats to unsold.
    let trip_id: Uuid = sqlx::query(
        "SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id
         WHERE r.code = 'BTG-CEB' ORDER BY t.departs_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .context("no trips — run `lulan-api seed` first")?
    .get(0);
    let seats: Vec<String> = sqlx::query(
        "SELECT cu.code FROM capacity_units cu
         JOIN trips t ON t.resource_id = cu.resource_id
         WHERE t.id = $1 AND cu.kind = 'seat' ORDER BY cu.code",
    )
    .bind(trip_id)
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|r| r.get(0))
    .collect();
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 0 WHERE trip_id = $1")
        .bind(trip_id)
        .execute(&pool)
        .await?;

    let total = match mode.as_str() {
        "paced" => (rate * duration_secs) as usize,
        _ => contenders,
    };
    println!(
        "mode: {mode} · trip {trip_id} · {} seats · {total} attempts{}",
        seats.len(),
        if mode == "paced" {
            format!(" ({rate}/s × {duration_secs}s, open loop)")
        } else {
            String::new()
        }
    );

    // Pre-compute every contender's move so tasks do nothing but HTTP.
    let mut rng = rand::rng();
    let plan: Vec<Task> = (0..total)
        .map(|i| {
            let (from, to) = SPANS[rng.random_range(0..SPANS.len())];
            Task {
                seat: seats[i % seats.len()].clone(),
                origin: CODES[from as usize],
                destination: CODES[to as usize],
                from,
                to,
                use_hold: rng.random_bool(hold_ratio),
            }
        })
        .collect();

    // HTTP/2 prior knowledge: thousands of contenders multiplex over a few
    // TCP connections instead of a 10k-SYN storm that overflows the OS
    // listen backlog — the same shape real SDK clients produce.
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .pool_max_idle_per_host(64)
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    match mode.as_str() {
        // Open loop: arrivals on a fixed clock, never waiting for earlier
        // responses — measured latency is service latency.
        "paced" => {
            let interval = std::time::Duration::from_nanos(1_000_000_000 / rate.max(1));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
            for task in plan {
                ticker.tick().await;
                let client = client.clone();
                let base = base_url.clone();
                tasks.spawn(run_task(client, base, trip_id, task));
            }
        }
        // Closed barrier burst: everyone fires at once.
        _ => {
            let barrier = Arc::new(tokio::sync::Barrier::new(total));
            for task in plan {
                let client = client.clone();
                let barrier = barrier.clone();
                let base = base_url.clone();
                tasks.spawn(async move {
                    barrier.wait().await;
                    run_task(client, base, trip_id, task).await
                });
            }
        }
    }

    let mut outcomes = Vec::with_capacity(total);
    while let Some(result) = tasks.join_next().await {
        outcomes.push(result?);
    }
    let wall = started.elapsed();

    // ---- Invariant: winners' spans per seat are pairwise disjoint --------
    let mut winners: HashMap<&str, u64> = HashMap::new();
    let mut overlap_violations = 0usize;
    for o in outcomes.iter().filter(|o| o.claimed) {
        let union = winners.entry(o.seat.as_str()).or_insert(0);
        if *union & o.mask != 0 {
            overlap_violations += 1;
            eprintln!("VIOLATION: overlapping winning claims on seat {}", o.seat);
        }
        *union |= o.mask;
    }

    // ---- Invariant: database masks equal the winners' unions -------------
    let mut db_violations = 0usize;
    let rows = sqlx::query(
        "SELECT cu.code, so.occupied_mask FROM seat_occupancy so
         JOIN capacity_units cu ON cu.id = so.unit_id
         WHERE so.trip_id = $1",
    )
    .bind(trip_id)
    .fetch_all(&pool)
    .await?;
    for row in &rows {
        let code: String = row.get(0);
        let db_mask: i64 = row.get(1);
        let expected = winners.get(code.as_str()).copied().unwrap_or(0);
        if db_mask as u64 != expected {
            db_violations += 1;
            eprintln!(
                "VIOLATION: seat {code} db mask {:#b} != winners' union {:#b}",
                db_mask, expected
            );
        }
    }

    // ---- Report -----------------------------------------------------------
    let claimed = outcomes.iter().filter(|o| o.claimed).count();
    let conflicts = outcomes.iter().filter(|o| !o.claimed && !o.error).count();
    let errors = outcomes.iter().filter(|o| o.error).count();
    let hold_errors = outcomes.iter().filter(|o| o.hold_error).count();

    let mut claim_lat: Vec<u128> = outcomes.iter().map(|o| o.claim_us).collect();
    claim_lat.sort_unstable();
    let mut hold_lat: Vec<u128> = outcomes.iter().filter_map(|o| o.hold_us).collect();
    hold_lat.sort_unstable();

    println!("\n== results ({mode}) ==");
    println!("wall time            {:.2}s", wall.as_secs_f64());
    println!(
        "throughput           {:.0} attempts/s",
        outcomes.len() as f64 / wall.as_secs_f64()
    );
    println!("claimed              {claimed}");
    println!("conflicts (409)      {conflicts}");
    println!("transport/5xx errors {errors}");
    if !hold_lat.is_empty() {
        println!("hold errors          {hold_errors}");
        println!(
            "hold latency (µs)    p50={} p95={} p99={}",
            percentile(&hold_lat, 50.0),
            percentile(&hold_lat, 95.0),
            percentile(&hold_lat, 99.0)
        );
    }
    println!(
        "claim latency (µs)   p50={} p95={} p99={}",
        percentile(&claim_lat, 50.0),
        percentile(&claim_lat, 95.0),
        percentile(&claim_lat, 99.0)
    );
    if mode == "paced" {
        let p95_ms = percentile(&claim_lat, 95.0) as f64 / 1000.0;
        println!(
            "PRD seat-lock target <20 ms: p95 = {:.2} ms → {}",
            p95_ms,
            if p95_ms < 20.0 { "PASS" } else { "MISS" }
        );
    }

    if overlap_violations == 0 && db_violations == 0 {
        println!(
            "\nINVARIANT OK: zero double-sells across {} attempts",
            outcomes.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "\nINVARIANT VIOLATED: {overlap_violations} overlaps, {db_violations} db mismatches"
        );
        Ok(ExitCode::FAILURE)
    }
}
