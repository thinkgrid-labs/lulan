//! "Not just ferries": the same engine sells a concert. Seats become show
//! sections (VIP / lower / upper), a pool becomes general admission, and a
//! single boarding segment is doors → end. Proves the capacity primitives
//! are domain-agnostic and that admission pools issue one bearer QR per
//! unit — while bulk pools (cargo, vehicle decks) still issue none.
//!
//! The events seed installs its OWN global fare ruleset, so this test runs
//! against a freshly-created, uniquely-named database rather than the
//! shared ferry one. Requires TEST_DATABASE_URL and CREATEDB privilege
//! (skips otherwise).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

const INTEGRATION_KEY: &str = "llk_test_integration_key_events_it";

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    api_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Provisions a throwaway database seeded with the events profile. Returns
/// the pool plus what the caller needs to drop it afterwards. `None` when
/// TEST_DATABASE_URL is unset or the role cannot create databases.
async fn provision_events_db() -> Option<(sqlx::PgPool, String, String)> {
    let admin_url = std::env::var("TEST_DATABASE_URL").ok()?;
    let (base, _db) = admin_url.rsplit_once('/')?;
    let server_url = format!("{base}/postgres");
    let db_name = format!("lulan_events_it_{}", Uuid::new_v4().simple());

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&server_url)
        .await
        .ok()?;
    if sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .is_err()
    {
        eprintln!("cannot CREATE DATABASE (needs CREATEDB) — skipping events_it");
        return None;
    }

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&format!("{base}/{db_name}"))
        .await
        .ok()?;
    lulan_api::MIGRATOR.run(&pool).await.unwrap();
    lulan_api::seed::seed_events(&pool).await.unwrap();
    Some((pool, server_url, db_name))
}

async fn drop_events_db(pool: sqlx::PgPool, server_url: &str, db_name: &str) {
    pool.close().await;
    if let Ok(admin) = PgPoolOptions::new()
        .max_connections(1)
        .connect(server_url)
        .await
    {
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
        ))
        .execute(&admin)
        .await;
    }
}

/// Books a VIP seat plus `ga` general-admission units on the first show
/// night, pays, and returns the issued tickets.
async fn book_and_ticket(app: &axum::Router, seat: &str, ga: u32) -> Vec<Value> {
    let date = (Utc::now() + Duration::days(2)).date_naive();
    let (status, search) = call(
        app,
        "GET",
        &format!("/v1/trips/search?origin=DOORS&destination=END&departure_date={date}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{search}");
    let trip_id = search["legs"][0]["trips"][0]["trip_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, quote) = call(
        app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip_id,
            "items": [
                {"unit_code": seat, "origin": "DOORS", "destination": "END", "passenger_type": "adult"},
                {"unit_code": "GENERAL_ADMISSION", "origin": "DOORS", "destination": "END",
                 "passenger_type": "adult", "quantity": ga},
            ],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{quote}");
    assert_eq!(quote["currency"], "USD");
    let quote_token = quote["quote_token"].as_str().unwrap().to_string();

    let (status, order) = call(
        app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id,
            "passengers": [{"full_name": "Maya Cruz", "type": "adult"}],
            "guest_contact": "maya@example.com",
            "quote_token": quote_token,
            "items": [
                {"unit_code": seat, "origin": "DOORS", "destination": "END",
                 "passenger": 0, "passenger_type": "adult"},
                {"unit_code": "GENERAL_ADMISSION", "origin": "DOORS", "destination": "END",
                 "passenger_type": "adult", "quantity": ga},
            ],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let order_id = order["order_id"].as_str().unwrap().to_string();
    let retrieval = order["retrieval_token"].as_str().unwrap().to_string();

    let (_, payment) = call(
        app,
        "POST",
        &format!("/v1/orders/{order_id}/payment?token={retrieval}"),
        Some(json!({})),
        None,
    )
    .await;
    let intent = payment["payment_intent_id"].as_str().unwrap();
    let (_, hook) = call(
        app,
        "POST",
        "/v1/payments/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
        Some(INTEGRATION_KEY),
    )
    .await;
    assert_eq!(hook["order_status"], "ticketed", "{hook}");

    let (status, tickets) = call(
        app,
        "GET",
        &format!("/v1/orders/{order_id}/tickets?token={retrieval}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tickets["tickets"].as_array().unwrap().clone()
}

#[tokio::test]
async fn concert_admission_pool_issues_one_bearer_qr_per_unit() {
    let Some((pool, server_url, db_name)) = provision_events_db().await else {
        return;
    };
    lulan_api::auth::bootstrap_admin_key(&pool, INTEGRATION_KEY)
        .await
        .unwrap();
    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);

    // One VIP seat + two general admission → three scannable QRs.
    let tickets = book_and_ticket(&app, "VIP-1", 2).await;
    assert_eq!(tickets.len(), 3, "1 named seat + 2 bearer GA");
    let ga: Vec<&Value> = tickets
        .iter()
        .filter(|t| t["unit_code"] == "GENERAL_ADMISSION")
        .collect();
    let seats: Vec<&Value> = tickets
        .iter()
        .filter(|t| t["unit_code"] == "VIP-1")
        .collect();
    assert_eq!(ga.len(), 2, "one bearer ticket per admission unit");
    assert_eq!(seats.len(), 1, "the reserved seat keeps its named holder");
    assert_eq!(seats[0]["passenger_name"], "Maya Cruz");
    // Bearer holder label, humanized from the pool code.
    assert_eq!(ga[0]["passenger_name"], "General Admission");
    // Every ticket is a signed LT1 token.
    assert!(
        tickets
            .iter()
            .all(|t| t["token"].as_str().unwrap().starts_with("LT1."))
    );

    // Flip the pool to bulk (admission = false): the same booking now issues
    // only the seat ticket — the flag, not the kind, is what draws the line.
    sqlx::query("UPDATE capacity_units SET admission = false WHERE code = 'GENERAL_ADMISSION'")
        .execute(&pool)
        .await
        .unwrap();
    let tickets = book_and_ticket(&app, "VIP-2", 2).await;
    assert_eq!(
        tickets.len(),
        1,
        "a non-admission pool issues no per-unit tickets"
    );
    assert_eq!(tickets[0]["unit_code"], "VIP-2");

    drop_events_db(pool, &server_url, &db_name).await;
}
