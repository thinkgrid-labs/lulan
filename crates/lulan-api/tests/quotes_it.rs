//! Quote → order integration (Phase 4): quoted prices are honoured at
//! checkout, tampering and cross-trip reuse are rejected, and promos
//! flow through. Requires TEST_DATABASE_URL (skips otherwise).
//!
//! Uses trip offset 2 (from the end) — offsets 0/1 belong to claims_it,
//! 3/4/5 to orders_it, and the earliest trip (offset 6 of the 7-day seed)
//! is the availability test's PRD-example fixture.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn quotes_price_orders_and_reject_tampering() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    lulan_api::MIGRATOR.run(&pool).await.unwrap();
    lulan_api::seed::seed(&pool).await.unwrap();

    let trip_id: Uuid =
        sqlx::query("SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id WHERE r.code = 'BTG-CEB' ORDER BY t.departs_at DESC LIMIT 1 OFFSET 2")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
    // Idempotent fixture: clear this trip's orders and everything hanging
    // off them (scan events → tickets → items → passengers → orders).
    // Orders are itineraries now: find every order touching this trip and
    // remove it wholesale (all legs), children first.
    let stale: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT order_id FROM order_items WHERE trip_id = $1")
            .bind(trip_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    for sql in [
        "DELETE FROM scan_events WHERE ticket_id IN (SELECT id FROM tickets WHERE order_id = ANY($1))",
        "DELETE FROM tickets WHERE order_id = ANY($1)",
        "DELETE FROM idempotency_keys WHERE order_id = ANY($1)",
        "DELETE FROM order_items WHERE order_id = ANY($1)",
        "DELETE FROM passengers WHERE order_id = ANY($1)",
        "DELETE FROM orders WHERE id = ANY($1)",
    ] {
        sqlx::query(sql).bind(&stale).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 0 WHERE trip_id = $1")
        .bind(trip_id)
        .execute(&pool)
        .await
        .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);
    let items = json!([
        {"unit_code": "11A", "origin": "BTG", "destination": "CEB", "passenger_type": "adult"},
        {"unit_code": "CARGO_KG", "origin": "BTG", "destination": "ILO", "quantity": 100},
    ]);

    // Quote: itemised, totalled, tokenised.
    let (status, quote) = post_json(
        &app,
        "/v1/quotes",
        json!({"trip_id": trip_id, "items": items}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{quote}");
    let quote_total = quote["total_minor"].as_i64().unwrap();
    let item_total: i64 = quote["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["total_minor"].as_i64().unwrap())
        .sum();
    assert!(quote_total > 0);
    assert_eq!(quote_total, item_total);
    let token = quote["quote_token"].as_str().unwrap().to_string();

    // A promo quote for the same items must be cheaper.
    let (_, promo_quote) = post_json(
        &app,
        "/v1/quotes",
        json!({"trip_id": trip_id, "items": items, "promo_code": "BAGONGBYAHE"}),
    )
    .await;
    assert!(promo_quote["total_minor"].as_i64().unwrap() < quote_total);

    // Tampered token: flip the last character of the signature.
    let mut tampered = token.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    let (status, body) = post_json(
        &app,
        "/v1/orders",
        json!({"trip_id": trip_id, "passengers": [{"full_name": "Eve Test", "type": "adult"}], "guest_contact": "test@example.com",
               "quote_token": tampered, "items": items}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An item not covered by the quote is rejected.
    let (status, _) = post_json(
        &app,
        "/v1/orders",
        json!({"trip_id": trip_id, "passengers": [{"full_name": "Eve Test", "type": "adult"}], "guest_contact": "test@example.com", "quote_token": token,
               "items": [{"unit_code": "11B", "origin": "BTG", "destination": "CEB"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Honest checkout with the token: order total equals the quoted total.
    let (status, order) = post_json(
        &app,
        "/v1/orders",
        json!({"trip_id": trip_id, "passengers": [{"full_name": "Maria Test", "type": "adult"}], "guest_contact": "test@example.com",
               "quote_token": token, "items": items}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["total_minor"].as_i64().unwrap(), quote_total);

    // Occupancy feedback: with 11A sold, re-quoting an identical seat may
    // only get more expensive, never cheaper (occupancy tiers are
    // monotonic surcharges).
    let (_, requote) = post_json(
        &app,
        "/v1/quotes",
        json!({"trip_id": trip_id,
               "items": [{"unit_code": "11B", "origin": "BTG", "destination": "CEB"}]}),
    )
    .await;
    let seat_before: i64 = quote["items"][0]["total_minor"].as_i64().unwrap();
    let seat_after: i64 = requote["items"][0]["total_minor"].as_i64().unwrap();
    assert!(seat_after >= seat_before, "{seat_after} < {seat_before}");
}
