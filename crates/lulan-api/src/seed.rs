//! Demo network seeder (`lulan-api seed`): a Philippine inter-island ferry
//! line — Batangas → Caticlan → Iloilo → Cebu (3 segments) — with seats,
//! a vehicle deck, and cargo capacity, sailing daily for the next 7 days.
//!
//! Idempotent: skips if the demo route already exists. The earliest trip
//! reproduces the PRD's example — seat 12A occupied on A→B and C→D, free
//! on B→C.

use anyhow::Context;
use chrono::{Duration, NaiveTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

const STOPS: [(&str, &str); 4] = [
    ("BTG", "Batangas"),
    ("CTC", "Caticlan"),
    ("ILO", "Iloilo"),
    ("CEB", "Cebu"),
];
const SEGMENTS: i16 = 3;
const DAYS: i64 = 7;

/// Per-stop schedule offsets (arrive, depart) in minutes from the trip's
/// origin departure — a long inter-island run with ~30-min dwells. Same
/// pattern serves both directions.
const STOP_SCHEDULE: [(i32, i32); 4] = [(0, 0), (240, 270), (510, 540), (780, 780)];

/// Default fare policy for the demo network. Idempotent, and upgrades in
/// place: if the active ruleset differs from the current default (e.g. a
/// newer engine added fields), it is deactivated and replaced.
async fn seed_fare_rules(pool: &PgPool) -> anyhow::Result<()> {
    let rules = serde_json::json!({
        "currency": "PHP",
        "base_fare_per_segment": {
            "economy": 15_000,
            "business": 30_000,
            "VEHICLE_DECK": 100_000,
            "CARGO_KG": 500,
        },
        "peak_weekdays": [4, 5, 6],
        "peak_surcharge_bp": 1_500,
        "occupancy_tiers": [
            {"min_occupancy_bp": 5_000, "surcharge_bp": 1_000},
            {"min_occupancy_bp": 8_000, "surcharge_bp": 2_500},
        ],
        "advance_purchase_tiers": [
            {"min_days": 7, "discount_bp": 1_000},
            {"min_days": 14, "discount_bp": 2_000},
        ],
        "promos": {"BAGONGBYAHE": 500},
        "round_trip_discount_bp": 1_000,
        "passenger_type_discounts": {
            "senior": 2_000,
            "pwd": 2_000,
            "child": 5_000,
            "infant": 10_000,
        },
    });
    // Fail fast if the JSON ever drifts from the engine's schema.
    let _: lulan_pricing::rules::FareRuleSet = serde_json::from_value(rules.clone())?;

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_LOCK)
        .execute(&mut *tx)
        .await?;

    let current: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT rules FROM fare_rules WHERE active ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if current.as_ref() == Some(&rules) {
        return Ok(());
    }

    // Exactly one ruleset is active at a time; both statements must land
    // together or a concurrent seeder can leave two.
    sqlx::query("UPDATE fare_rules SET active = false WHERE active")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO fare_rules (id, active, rules) VALUES ($1, true, $2)")
        .bind(Uuid::new_v4())
        .bind(rules)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    println!("seeded default fare rules");
    Ok(())
}

/// Demo add-on catalog. Idempotent (upsert by code).
async fn seed_ancillaries(pool: &PgPool) -> anyhow::Result<()> {
    // (code, name, kind, price_minor, per, scope)
    let catalog: [(&str, &str, &str, i64, &str, &str); 4] = [
        (
            "BAG20",
            "Checked bag 20 kg",
            "baggage",
            25_000,
            "passenger",
            "journey",
        ),
        (
            "MEAL_STD",
            "Onboard meal",
            "meal",
            12_000,
            "passenger",
            "journey",
        ),
        (
            "INSURE",
            "Travel insurance",
            "insurance",
            15_000,
            "passenger",
            "itinerary",
        ),
        (
            "PRIORITY",
            "Priority boarding",
            "service",
            8_000,
            "passenger",
            "journey",
        ),
    ];
    for (code, name, kind, price, per, scope) in catalog {
        sqlx::query(
            "INSERT INTO ancillaries (id, code, name, kind, price_minor, per, scope)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (code) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(code)
        .bind(name)
        .bind(kind)
        .bind(price)
        .bind(per)
        .bind(scope)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Arbitrary but fixed: the advisory-lock key that serialises seeding.
const SEED_LOCK: i64 = 0x_1F1A_5EED;

pub async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    seed_fare_rules(pool).await?;
    seed_ancillaries(pool).await?;

    // Every step here is a check-then-act, and each one used to be a read
    // followed by an unguarded write. N callers starting against an empty
    // database all saw "empty" and all inserted, producing duplicate-key
    // errors — which integration suites hit whenever one runs against a
    // fresh database without another having seeded first.
    //
    // Each step now takes the SAME transaction-scoped advisory lock, so
    // seeding is serialised as a whole. The lock is released on commit or
    // rollback, so a seeder that fails cannot wedge the others.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_LOCK)
        .execute(&mut *tx)
        .await?;

    let already =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM routes WHERE code = 'BTG-CEB'")
            .fetch_one(&mut *tx)
            .await?;
    if already > 0 {
        tx.commit().await?;
        println!("demo network already seeded");
        seed_return_route(pool).await?;
        return Ok(());
    }

    // Locations and route.
    let mut location_ids = Vec::new();
    for (code, name) in STOPS {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO locations (id, code, name, timezone) VALUES ($1, $2, $3, 'Asia/Manila')",
        )
        .bind(id)
        .bind(code)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        location_ids.push(id);
    }

    // The operator (carrier) running the service.
    let operator_id = Uuid::new_v4();
    sqlx::query("INSERT INTO operators (id, code, name) VALUES ($1, 'LUL', 'Lulan Ferries')")
        .bind(operator_id)
        .execute(&mut *tx)
        .await?;

    let route_id = Uuid::new_v4();
    sqlx::query("INSERT INTO routes (id, code, name) VALUES ($1, 'BTG-CEB', 'Batangas – Cebu')")
        .bind(route_id)
        .execute(&mut *tx)
        .await?;
    for (index, location_id) in location_ids.iter().enumerate() {
        let (arrive, depart) = STOP_SCHEDULE[index];
        sqlx::query(
            "INSERT INTO route_stops (route_id, stop_index, location_id, arrive_offset_min, depart_offset_min)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(route_id)
        .bind(index as i16)
        .bind(location_id)
        .bind(arrive)
        .bind(depart)
        .execute(&mut *tx)
        .await?;
    }

    // The vessel: rows 1–3 business, 4–13 economy, letters A–D (52 seats),
    // plus a 20-slot vehicle deck and 5000 kg cargo pool per segment.
    let resource_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO resources (id, code, name, kind) VALUES ($1, 'MV-LULAN-1', 'MV Lulan One', 'ferry')",
    )
    .bind(resource_id)
    .execute(&mut *tx)
    .await?;

    let mut seat_ids = Vec::new();
    for row in 1..=13 {
        for letter in ['A', 'B', 'C', 'D'] {
            let id = Uuid::new_v4();
            let fare_class = if row <= 3 { "business" } else { "economy" };
            sqlx::query(
                "INSERT INTO capacity_units (id, resource_id, kind, code, fare_class)
                 VALUES ($1, $2, 'seat', $3, $4)",
            )
            .bind(id)
            .bind(resource_id)
            .bind(format!("{row}{letter}"))
            .bind(fare_class)
            .execute(&mut *tx)
            .await?;
            seat_ids.push((id, format!("{row}{letter}")));
        }
    }

    let mut pool_ids = Vec::new();
    for (code, capacity) in [("VEHICLE_DECK", 20), ("CARGO_KG", 5000)] {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO capacity_units (id, resource_id, kind, code, pool_capacity)
             VALUES ($1, $2, 'pool', $3, $4)",
        )
        .bind(id)
        .bind(resource_id)
        .bind(code)
        .bind(capacity)
        .execute(&mut *tx)
        .await?;
        pool_ids.push((id, capacity));
    }

    // Daily sailings for the next 7 days, 08:00 Manila time (00:00 UTC).
    let today = Utc::now().date_naive();
    let mut first_trip_id = None;
    for day in 0..DAYS {
        let service_date = today + Duration::days(day);
        let departs_at = service_date
            .and_time(NaiveTime::from_hms_opt(0, 0, 0).context("valid time")?)
            .and_utc();
        let trip_id = Uuid::new_v4();
        first_trip_id.get_or_insert(trip_id);

        sqlx::query(
            "INSERT INTO trips (id, route_id, resource_id, operator_id, service_number, service_date, departs_at, segment_count)
             VALUES ($1, $2, $3, $4, 'LUL 501', $5, $6, $7)",
        )
        .bind(trip_id)
        .bind(route_id)
        .bind(resource_id)
        .bind(operator_id)
        .bind(service_date)
        .bind(departs_at)
        .bind(SEGMENTS)
        .execute(&mut *tx)
        .await?;

        for (seat_id, _) in &seat_ids {
            sqlx::query(
                "INSERT INTO seat_occupancy (trip_id, unit_id, occupied_mask) VALUES ($1, $2, 0)",
            )
            .bind(trip_id)
            .bind(seat_id)
            .execute(&mut *tx)
            .await?;
        }
        for (pool_unit_id, capacity) in &pool_ids {
            sqlx::query(
                "INSERT INTO pool_occupancy (trip_id, unit_id, remaining)
                 VALUES ($1, $2, array_fill($3::int, ARRAY[$4::int]))",
            )
            .bind(trip_id)
            .bind(pool_unit_id)
            .bind(capacity)
            .bind(i32::from(SEGMENTS))
            .execute(&mut *tx)
            .await?;
        }
    }

    // PRD example on the earliest trip: seat 12A occupied on segments 0 and
    // 2 (BTG→CTC and ILO→CEB), free on segment 1 (CTC→ILO). Mask 0b101.
    let first_trip = first_trip_id.context("at least one trip seeded")?;
    let seat_12a = &seat_ids
        .iter()
        .find(|(_, code)| code == "12A")
        .context("seat 12A exists")?
        .0;
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 5 WHERE trip_id = $1 AND unit_id = $2")
        .bind(first_trip)
        .bind(seat_12a)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    println!(
        "seeded demo network: BTG→CTC→ILO→CEB, 52 seats + 2 pools, {DAYS} daily departures (first trip {first_trip})"
    );
    seed_return_route(pool).await?;
    Ok(())
}

/// The return direction (CEB→ILO→CTC→BTG, 12:00 UTC daily) — what makes
/// round-trip itineraries possible (Phase 6.5). Idempotent; also upgrades
/// databases seeded before the return route existed.
async fn seed_return_route(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_LOCK)
        .execute(&mut *tx)
        .await?;

    let already =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM routes WHERE code = 'CEB-BTG'")
            .fetch_one(&mut *tx)
            .await?;
    if already > 0 {
        return Ok(());
    }

    let resource_id: Uuid =
        sqlx::query_scalar("SELECT id FROM resources WHERE code = 'MV-LULAN-1'")
            .fetch_one(&mut *tx)
            .await
            .context("outbound network must be seeded first")?;
    let operator_id: Uuid = sqlx::query_scalar("SELECT id FROM operators WHERE code = 'LUL'")
        .fetch_one(&mut *tx)
        .await
        .context("operator must be seeded first")?;

    let route_id = Uuid::new_v4();
    sqlx::query("INSERT INTO routes (id, code, name) VALUES ($1, 'CEB-BTG', 'Cebu – Batangas')")
        .bind(route_id)
        .execute(&mut *tx)
        .await?;
    for (index, (code, _)) in STOPS.iter().rev().enumerate() {
        let (arrive, depart) = STOP_SCHEDULE[index];
        sqlx::query(
            "INSERT INTO route_stops (route_id, stop_index, location_id, arrive_offset_min, depart_offset_min)
             SELECT $1, $2, id, $4, $5 FROM locations WHERE code = $3",
        )
        .bind(route_id)
        .bind(index as i16)
        .bind(code)
        .bind(arrive)
        .bind(depart)
        .execute(&mut *tx)
        .await?;
    }

    let units: Vec<(Uuid, Option<i32>)> =
        sqlx::query_as("SELECT id, pool_capacity FROM capacity_units WHERE resource_id = $1")
            .bind(resource_id)
            .fetch_all(&mut *tx)
            .await?;

    let today = Utc::now().date_naive();
    for day in 0..DAYS {
        let service_date = today + Duration::days(day);
        let departs_at = service_date
            .and_time(NaiveTime::from_hms_opt(12, 0, 0).context("valid time")?)
            .and_utc();
        let trip_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO trips (id, route_id, resource_id, operator_id, service_number, service_date, departs_at, segment_count)
             VALUES ($1, $2, $3, $4, 'LUL 502', $5, $6, $7)",
        )
        .bind(trip_id)
        .bind(route_id)
        .bind(resource_id)
        .bind(operator_id)
        .bind(service_date)
        .bind(departs_at)
        .bind(SEGMENTS)
        .execute(&mut *tx)
        .await?;

        for (unit_id, pool_capacity) in &units {
            match pool_capacity {
                None => {
                    sqlx::query(
                        "INSERT INTO seat_occupancy (trip_id, unit_id, occupied_mask) VALUES ($1, $2, 0)",
                    )
                    .bind(trip_id)
                    .bind(unit_id)
                    .execute(&mut *tx)
                    .await?;
                }
                Some(capacity) => {
                    sqlx::query(
                        "INSERT INTO pool_occupancy (trip_id, unit_id, remaining)
                         VALUES ($1, $2, array_fill($3::int, ARRAY[$4::int]))",
                    )
                    .bind(trip_id)
                    .bind(unit_id)
                    .bind(capacity)
                    .bind(i32::from(SEGMENTS))
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
    }

    tx.commit().await?;
    println!("seeded return route: CEB→ILO→CTC→BTG, {DAYS} daily departures");
    Ok(())
}
