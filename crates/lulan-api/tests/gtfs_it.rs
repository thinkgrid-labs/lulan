//! GTFS importer (Phase 7): a mini feed (1 agency, 3 ports, 1 route in
//! both directions, daily service) imports into a searchable, bookable
//! network with real schedule times. Requires TEST_DATABASE_URL.

use axum::body::Body;
use axum::http::Request;
use chrono::{Duration, Utc};
use lulan_api::gtfs::{GtfsOptions, import};
use lulan_api::state::AppState;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

#[tokio::test]
async fn gtfs_feed_imports_into_a_bookable_network() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .unwrap();
    lulan_api::MIGRATOR.run(&pool).await.unwrap();
    lulan_api::seed::seed(&pool).await.unwrap(); // fare rules for pricing

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gtfs");
    import(
        &pool,
        &dir,
        GtfsOptions {
            days: 7,
            seats: 12,
            vessel: None,
        },
    )
    .await
    .expect("import succeeds");
    // Idempotent: re-import adds nothing new and does not error.
    import(
        &pool,
        &dir,
        GtfsOptions {
            days: 7,
            seats: 12,
            vessel: None,
        },
    )
    .await
    .expect("re-import is idempotent");

    // Two directions → two route patterns.
    let patterns: i64 =
        sqlx::query_scalar("SELECT count(*) FROM routes WHERE code IN ('NX-0', 'NX-1')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(patterns, 2);

    // Searchable over HTTP with real schedule + operator identity.
    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);
    let date = (Utc::now().date_naive() + Duration::days(2)).to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/trips/search?origin=PRT1&destination=PRT3&departure_date={date}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let trips = body["legs"][0]["trips"].as_array().unwrap();
    assert_eq!(trips.len(), 1, "one NX-AM departure that day: {body}");
    let trip = &trips[0];
    assert_eq!(trip["operator"]["name"], "Island Express");
    assert_eq!(trip["service_number"], "NX-AM");
    // 06:00 → 11:00 = 300 minutes end to end.
    assert_eq!(trip["duration_minutes"], 300);
    let economy = &trip["seats"][0];
    assert_eq!(economy["total"], 12);
    assert_eq!(economy["available"], 12);

    // The reverse direction is its own searchable route.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/trips/search?origin=PRT3&destination=PRT1&departure_date={date}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["legs"][0]["trips"].as_array().unwrap().len(),
        1,
        "return pattern searchable: {body}"
    );

    // Mid-route span prices and books (proves offsets + spans line up).
    let trip_id = trip["trip_id"].as_str().unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/quotes")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "trip_id": trip_id,
                        "items": [{"unit_code": "1A", "origin": "PRT2", "destination": "PRT3"}],
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "imported network quotes");
}
