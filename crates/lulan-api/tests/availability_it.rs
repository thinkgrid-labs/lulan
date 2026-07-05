//! End-to-end availability test against a real Postgres (the Phase 1 exit
//! criterion). Skips when TEST_DATABASE_URL is unset so `cargo test` stays
//! green without infrastructure; CI always sets it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::Value;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn seat<'a>(availability: &'a Value, code: &str) -> &'a Value {
    availability["seats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["code"] == code)
        .unwrap_or_else(|| panic!("seat {code} in response"))
}

#[tokio::test]
async fn availability_answers_the_prd_example_over_http() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping integration test");
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect to test database");
    lulan_api::MIGRATOR.run(&pool).await.expect("migrations");
    lulan_api::seed::seed(&pool).await.expect("seed");

    // The seeder marks seat 12A on the earliest trip with mask 0b101.
    // Restore that fixture explicitly so this test is self-healing even if
    // another run has touched the trip.
    let row =
        sqlx::query("SELECT t.id::text, t.service_date::text FROM trips t JOIN routes r ON r.id = t.route_id WHERE r.code = 'BTG-CEB' ORDER BY t.departs_at LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let trip_id: String = row.get(0);
    let date: String = row.get(1);
    let trip_uuid = uuid::Uuid::parse_str(&trip_id).unwrap();
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 0 WHERE trip_id = $1")
        .bind(trip_uuid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 5
         FROM capacity_units cu
         WHERE so.trip_id = $1 AND cu.id = so.unit_id AND cu.code = '12A'",
    )
    .bind(trip_uuid)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE pool_occupancy po SET remaining = array_fill(cu.pool_capacity, ARRAY[3])
         FROM capacity_units cu WHERE cu.id = po.unit_id AND po.trip_id = $1",
    )
    .bind(trip_uuid)
    .execute(&pool)
    .await
    .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);

    // Search: full-journey availability must see 12A as unavailable and the
    // fare summary must count seats for the exact requested span.
    let (status, body) = get_json(
        &app,
        &format!("/v1/trips/search?origin=BTG&destination=CEB&date={date}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let trips = body["trips"].as_array().unwrap();
    assert!(!trips.is_empty(), "search must find the seeded sailing");
    let hit = &trips[0];
    assert_eq!(hit["trip_id"].as_str().unwrap(), trip_id);
    assert_eq!(hit["from_index"], 0);
    assert_eq!(hit["to_index"], 3);
    let economy = hit["seats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["fare_class"] == "economy")
        .expect("economy fare row");
    // 40 economy seats, 12A blocked for the full journey.
    assert_eq!(economy["total"], 40);
    assert_eq!(economy["available"], 39);
    let deck = hit["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == "VEHICLE_DECK")
        .expect("vehicle deck pool");
    assert_eq!(deck["remaining"], 20);

    // PRD example, per segment: occupied BTG→CTC, free CTC→ILO, occupied ILO→CEB.
    for (origin, destination, expect_available) in [
        ("BTG", "CTC", false),
        ("CTC", "ILO", true),
        ("ILO", "CEB", false),
        ("BTG", "CEB", false),
    ] {
        let (status, body) = get_json(
            &app,
            &format!("/v1/trips/{trip_id}/availability?origin={origin}&destination={destination}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            seat(&body, "12A")["available"],
            expect_available,
            "seat 12A {origin}→{destination}"
        );
        // A business seat is untouched and available on every span.
        assert_eq!(seat(&body, "1A")["available"], true);
    }

    // Bad requests: reversed journey and unknown stop.
    let (status, _) = get_json(
        &app,
        &format!("/v1/trips/{trip_id}/availability?origin=CEB&destination=BTG"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get_json(
        &app,
        &format!("/v1/trips/{trip_id}/availability?origin=BTG&destination=XXX"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown trip is a 404.
    let (status, _) = get_json(
        &app,
        "/v1/trips/00000000-0000-0000-0000-000000000000/availability?origin=BTG&destination=CEB",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
