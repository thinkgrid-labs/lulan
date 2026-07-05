//! Concurrency integration tests for holds and claims (Phase 2 exit
//! criteria, CI-scale). The full 10k-contender run lives in lulan-loadgen;
//! this proves the invariant through the real HTTP surface with hundreds of
//! simultaneous contenders on one Postgres.
//!
//! Requires TEST_DATABASE_URL (skips otherwise). The hold-flow test
//! additionally requires TEST_REDIS_URL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use uuid::Uuid;

/// Each test gets its own trip (`offset` from the latest) so parallel tests
/// in this binary never reset each other's fixture. Trips are taken from
/// the END of the schedule so the availability test's first-trip fixture
/// stays untouched.
async fn setup(offset: i64) -> Option<(PgPool, Uuid)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return None;
    };
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await
        .expect("connect to test database");
    lulan_api::MIGRATOR.run(&pool).await.expect("migrations");
    lulan_api::seed::seed(&pool).await.expect("seed");

    let trip_id: Uuid =
        sqlx::query("SELECT id FROM trips ORDER BY departs_at DESC LIMIT 1 OFFSET $1")
            .bind(offset)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 0 WHERE trip_id = $1")
        .bind(trip_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE pool_occupancy po
         SET remaining = array_fill(cu.pool_capacity, ARRAY[3])
         FROM capacity_units cu
         WHERE cu.id = po.unit_id AND po.trip_id = $1",
    )
    .bind(trip_id)
    .execute(&pool)
    .await
    .unwrap();

    Some((pool, trip_id))
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claims_never_double_sell() {
    let Some((pool, trip_id)) = setup(0).await else {
        return;
    };
    let app = lulan_api::router(AppState::new(Some(pool.clone()), None));

    // Race 1: 500 contenders, same seat, same full-journey span.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..500 {
        let app = app.clone();
        tasks.spawn(async move {
            let (status, _) = post_json(
                &app,
                &format!("/v1/trips/{trip_id}/claims"),
                json!({"unit_code": "1A", "origin": "BTG", "destination": "CEB"}),
            )
            .await;
            status
        });
    }
    let mut created = 0;
    let mut conflicts = 0;
    while let Some(status) = tasks.join_next().await {
        match status.unwrap() {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => conflicts += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one contender may win the seat");
    assert_eq!(conflicts, 499);

    // Race 2: 300 contenders on one seat with random spans. Winners' spans
    // must be pairwise disjoint and match the final database mask.
    let spans = [(0u8, 1u8), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
    let codes = ["BTG", "CTC", "ILO", "CEB"];
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..300 {
        let app = app.clone();
        let (from, to) = spans[i % spans.len()];
        tasks.spawn(async move {
            let (status, _) = post_json(
                &app,
                &format!("/v1/trips/{trip_id}/claims"),
                json!({"unit_code": "2B", "origin": codes[from as usize], "destination": codes[to as usize]}),
            )
            .await;
            (status, from, to)
        });
    }
    let mut union: u64 = 0;
    while let Some(result) = tasks.join_next().await {
        let (status, from, to) = result.unwrap();
        if status == StatusCode::CREATED {
            let mask = (((1u64 << (to - from)) - 1) << from) & 0b111;
            assert_eq!(union & mask, 0, "winning spans must be disjoint");
            union |= mask;
        }
    }
    let db_mask: i64 = sqlx::query(
        "SELECT so.occupied_mask FROM seat_occupancy so
         JOIN capacity_units cu ON cu.id = so.unit_id
         WHERE so.trip_id = $1 AND cu.code = '2B'",
    )
    .bind(trip_id)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(
        db_mask as u64, union,
        "database mask must equal winners' union"
    );

    // Race 3: pool exhaustion — 100 contenders for 20 vehicle-deck slots.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..100 {
        let app = app.clone();
        tasks.spawn(async move {
            let (status, _) = post_json(
                &app,
                &format!("/v1/trips/{trip_id}/claims"),
                json!({"unit_code": "VEHICLE_DECK", "origin": "BTG", "destination": "CEB", "quantity": 1}),
            )
            .await;
            status
        });
    }
    let mut pool_wins = 0;
    while let Some(status) = tasks.join_next().await {
        if status.unwrap() == StatusCode::CREATED {
            pool_wins += 1;
        }
    }
    assert_eq!(pool_wins, 20, "exactly the pool capacity may be claimed");
    let remaining: Vec<i32> = sqlx::query(
        "SELECT po.remaining FROM pool_occupancy po
         JOIN capacity_units cu ON cu.id = po.unit_id
         WHERE po.trip_id = $1 AND cu.code = 'VEHICLE_DECK'",
    )
    .bind(trip_id)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(remaining, vec![0, 0, 0]);
}

#[tokio::test]
async fn hold_flow_protects_spans_and_feeds_claims() {
    let Some((pool, trip_id)) = setup(1).await else {
        return;
    };
    let Ok(redis_url) = std::env::var("TEST_REDIS_URL") else {
        eprintln!("TEST_REDIS_URL not set — skipping hold flow test");
        return;
    };
    let mut redis = redis::Client::open(redis_url.as_str())
        .unwrap()
        .get_connection_manager()
        .await
        .expect("connect to test redis");
    // TEST_REDIS_URL points at a dedicated instance; start from a clean
    // slate so holds from previous runs (10-minute TTL) can't collide.
    let _: () = redis::cmd("FLUSHDB").query_async(&mut redis).await.unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), Some(redis)));
    let holds_uri = format!("/v1/trips/{trip_id}/holds");

    // Hold BTG→ILO on 3C.
    let (status, hold) = post_json(
        &app,
        &holds_uri,
        json!({"unit_code": "3C", "origin": "BTG", "destination": "ILO"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let hold_id = hold["hold_id"].as_str().unwrap().to_string();

    // Overlapping hold must be rejected; non-overlapping must be admitted.
    let (status, _) = post_json(
        &app,
        &holds_uri,
        json!({"unit_code": "3C", "origin": "CTC", "destination": "CEB"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "overlapping hold");
    let (status, tail_hold) = post_json(
        &app,
        &holds_uri,
        json!({"unit_code": "3C", "origin": "ILO", "destination": "CEB"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "disjoint span is holdable");

    // Claim with the hold; the winning claim releases it.
    let (status, _) = post_json(
        &app,
        &format!("/v1/trips/{trip_id}/claims"),
        json!({"unit_code": "3C", "origin": "BTG", "destination": "ILO", "hold_id": hold_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A stale/foreign hold id must be rejected at claim time.
    let (status, _) = post_json(
        &app,
        &format!("/v1/trips/{trip_id}/claims"),
        json!({"unit_code": "3D", "origin": "BTG", "destination": "CTC",
               "hold_id": "00000000-0000-0000-0000-000000000000"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Releasing the tail hold frees its span for another session.
    let tail_id = tail_hold["hold_id"].as_str().unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/holds/{tail_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (status, _) = post_json(
        &app,
        &holds_uri,
        json!({"unit_code": "3C", "origin": "ILO", "destination": "CEB"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "released span is holdable again"
    );

    // Holding an already-sold span must 409 without touching Redis state.
    let (status, _) = post_json(
        &app,
        &holds_uri,
        json!({"unit_code": "3C", "origin": "BTG", "destination": "CTC"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "sold span cannot be held");
}
