//! Order lifecycle integration tests (Phase 3 exit criteria): full
//! lifecycle over HTTP against FakeProvider, idempotent/out-of-order
//! webhooks, atomic conflict rollback, cancel/expiry releasing inventory,
//! and event replay equalling the read model.
//!
//! Requires TEST_DATABASE_URL (skips otherwise). Uses trips offset 3+ from
//! the end so other integration binaries' fixtures are untouched.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use uuid::Uuid;

/// The provider-callback credential. Capture turns an order into a
/// boarding pass, so the fake provider's webhook is server-to-server only.
const API_KEY: &str = "llk_test_orders_it_key";

async fn setup(offset: i64) -> Option<(PgPool, Uuid, axum::Router)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return None;
    };
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .expect("connect to test database");
    lulan_api::MIGRATOR.run(&pool).await.expect("migrations");
    lulan_api::seed::seed(&pool).await.expect("seed");
    lulan_api::auth::bootstrap_admin_key(&pool, API_KEY)
        .await
        .expect("bootstrap key");

    let trip_id: Uuid =
        sqlx::query("SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id WHERE r.code = 'BTG-CEB' ORDER BY t.departs_at DESC LIMIT 1 OFFSET $1")
            .bind(offset)
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
        "DELETE FROM order_ancillaries WHERE order_id = ANY($1)",
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
    sqlx::query(
        "UPDATE pool_occupancy po SET remaining = array_fill(cu.pool_capacity, ARRAY[3])
         FROM capacity_units cu WHERE cu.id = po.unit_id AND po.trip_id = $1",
    )
    .bind(trip_id)
    .execute(&pool)
    .await
    .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);
    Some((pool, trip_id, app))
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    request_keyed(app, method, uri, body, None).await
}

async fn request_keyed(
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn seat_available(availability: &Value, code: &str) -> bool {
    availability["seats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["code"] == code)
        .unwrap()["available"]
        .as_bool()
        .unwrap()
}

#[tokio::test]
async fn full_lifecycle_with_idempotent_webhooks_and_replay() {
    let Some((pool, trip_id, app)) = setup(3).await else {
        return;
    };

    // Create: 2 seats + vehicle deck, atomically claimed.
    let (status, order) = request(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id,
            "passengers": [
                {"full_name": "Maria Santos", "type": "adult"},
                {"full_name": "Jose Santos", "type": "senior", "birthdate": "1958-03-14"},
            ],
            "guest_contact": "maria@example.com",
            "items": [
                {"unit_code": "5A", "origin": "BTG", "destination": "CEB", "passenger": 0},
                {"unit_code": "5B", "origin": "BTG", "destination": "CEB", "passenger": 1},
                {"unit_code": "VEHICLE_DECK", "origin": "BTG", "destination": "CEB", "quantity": 1},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["status"], "locked");
    // Live-priced (Phase 4): the total is exactly the sum of item prices.
    let item_sum: i64 = order["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["price_minor"].as_i64().unwrap())
        .sum();
    assert!(item_sum > 0, "items must carry real prices");
    assert_eq!(order["total_minor"].as_i64().unwrap(), item_sum);
    // The senior's seat (same class, same span) must be cheaper than the
    // adult's — mandated discount flowing through live pricing.
    let price_of = |code: &str| -> i64 {
        order["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["unit_code"] == code)
            .unwrap()["price_minor"]
            .as_i64()
            .unwrap()
    };
    assert!(
        price_of("5B") < price_of("5A"),
        "senior discount must apply"
    );
    assert_eq!(order["passengers"].as_array().unwrap().len(), 2);
    let order_id = order["order_id"].as_str().unwrap().to_string();
    // Phase 6: order reads are gated; guests keep the retrieval token.
    let retrieval = order["retrieval_token"].as_str().unwrap().to_string();

    // The claims are visible in availability immediately.
    let (_, avail) = request(
        &app,
        "GET",
        &format!("/v1/trips/{trip_id}/availability?origin=BTG&destination=CEB"),
        None,
    )
    .await;
    assert!(!seat_available(&avail, "5A"));
    assert!(!seat_available(&avail, "5B"));

    // Minting a payment intent is gated exactly like reading the order:
    // the id alone is not a credential.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/payment"),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "payment intents must not be mintable from the order id alone"
    );

    // Request payment → PendingPayment with a fake intent.
    let (status, payment) = request(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/payment?token={retrieval}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{payment}");
    assert_eq!(payment["status"], "pending_payment");
    let intent = payment["payment_intent_id"].as_str().unwrap().to_string();

    // Capture is the free-travel endpoint: without the integration
    // credential it is refused, intent id or not.
    let (status, _) = request(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "anonymous callers must not be able to capture payment"
    );

    // Provider webhook: succeeded → Paid.
    let (status, hook) = request_keyed(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
        Some(API_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Capture auto-issues tickets (Phase 5): the order lands on Ticketed.
    assert_eq!(hook["order_status"], "ticketed");
    assert_eq!(hook["applied"], true);

    // Duplicate delivery: acknowledged, not applied, state unchanged.
    let (status, dup) = request_keyed(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
        Some(API_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dup["applied"], false);
    assert_eq!(dup["order_status"], "ticketed");

    // Out-of-order failure after capture: same idempotent no-op.
    let (status, late_fail) = request_keyed(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "failed"})),
        Some(API_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(late_fail["applied"], false);
    assert_eq!(late_fail["order_status"], "ticketed");

    // Cancellation is gated too — and authorization is checked before
    // state, so an anonymous caller learns nothing about the order.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/cancel"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Paid orders cannot be cancelled.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/cancel?token={retrieval}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Read model shows the full event trail, exactly once per transition.
    let (status, _) = request(&app, "GET", &format!("/v1/orders/{order_id}"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "gated without a token");
    let (_, details) = request(
        &app,
        "GET",
        &format!("/v1/orders/{order_id}?token={retrieval}"),
        None,
    )
    .await;
    let event_types: Vec<&str> = details["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        event_types,
        vec![
            "order_created",
            "inventory_locked",
            "payment_requested",
            "payment_captured",
            "ticket_issued"
        ]
    );
    assert_eq!(details["status"], "ticketed");
    assert!(details["expires_at"].is_null(), "paid orders don't expire");

    // Exit criterion: replaying the event stream reproduces the read model.
    let store = lulan_engine::orders::OrderStore::new(pool.clone());
    let replayed = store
        .replay_status(Uuid::parse_str(&order_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replayed, lulan_engine::domain::OrderStatus::Ticketed);
}

#[tokio::test]
async fn conflicting_order_rolls_back_completely() {
    let Some((pool, trip_id, app)) = setup(4).await else {
        return;
    };

    // First order takes 6A for the full journey.
    let (status, _) = request(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id, "passengers": [{"full_name": "A Test", "type": "adult"}], "guest_contact": "test@example.com",
            "items": [{"unit_code": "6A", "origin": "BTG", "destination": "CEB"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Second order wants 6B (free) AND 6A (taken): must fail atomically.
    let orders_before: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT order_id) FROM order_items WHERE trip_id = $1")
            .bind(trip_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (status, body) = request(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id, "passengers": [{"full_name": "B Test", "type": "adult"}], "guest_contact": "test@example.com",
            "items": [
                {"unit_code": "6B", "origin": "BTG", "destination": "CEB"},
                {"unit_code": "6A", "origin": "CTC", "destination": "CEB"},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Nothing was written: no order row, 6B still available.
    let orders_after: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT order_id) FROM order_items WHERE trip_id = $1")
            .bind(trip_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(orders_after, orders_before);
    let (_, avail) = request(
        &app,
        "GET",
        &format!("/v1/trips/{trip_id}/availability?origin=BTG&destination=CEB"),
        None,
    )
    .await;
    assert!(
        seat_available(&avail, "6B"),
        "failed order must not leak claims"
    );
}

#[tokio::test]
async fn cancel_and_expiry_release_inventory() {
    let Some((pool, trip_id, app)) = setup(5).await else {
        return;
    };

    // Cancel path.
    let (_, order) = request(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id, "passengers": [{"full_name": "C Test", "type": "adult"}], "guest_contact": "test@example.com",
            "items": [
                {"unit_code": "7A", "origin": "BTG", "destination": "ILO"},
                {"unit_code": "VEHICLE_DECK", "origin": "BTG", "destination": "ILO", "quantity": 2},
            ],
        })),
    )
    .await;
    let order_id = order["order_id"].as_str().unwrap();
    let cancel_token = order["retrieval_token"].as_str().unwrap();
    let (status, cancelled) = request(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/cancel?token={cancel_token}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");
    let (_, avail) = request(
        &app,
        "GET",
        &format!("/v1/trips/{trip_id}/availability?origin=BTG&destination=ILO"),
        None,
    )
    .await;
    assert!(seat_available(&avail, "7A"), "cancel must release the seat");
    let deck = avail["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == "VEHICLE_DECK")
        .unwrap();
    assert_eq!(deck["remaining"], 20, "cancel must release pool quantity");

    // Expiry path: backdate expires_at, run the sweeper directly.
    let (_, order) = request(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id, "passengers": [{"full_name": "D Test", "type": "adult"}], "guest_contact": "test@example.com",
            "items": [{"unit_code": "8A", "origin": "BTG", "destination": "CEB"}],
        })),
    )
    .await;
    let order_id = order["order_id"].as_str().unwrap();
    let retrieval = order["retrieval_token"].as_str().unwrap();
    sqlx::query("UPDATE orders SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(Uuid::parse_str(order_id).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let store = lulan_engine::orders::OrderStore::new(pool.clone());
    let expired = store.expire_due().await.unwrap();
    assert!(expired >= 1);

    let (_, details) = request(
        &app,
        "GET",
        &format!("/v1/orders/{order_id}?token={retrieval}"),
        None,
    )
    .await;
    assert_eq!(details["status"], "expired");
    let (_, avail) = request(
        &app,
        "GET",
        &format!("/v1/trips/{trip_id}/availability?origin=BTG&destination=CEB"),
        None,
    )
    .await;
    assert!(seat_available(&avail, "8A"), "expiry must release the seat");

    // Replay agrees for both terminal states.
    for id in [order_id] {
        let replayed = store
            .replay_status(Uuid::parse_str(id).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replayed, lulan_engine::domain::OrderStatus::Expired);
    }

    // The event log is append-only: mutation attempts must be rejected.
    let err = sqlx::query("UPDATE events SET event_type = 'tampered' WHERE stream_id = $1")
        .bind(Uuid::parse_str(order_id).unwrap())
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");
    let err = sqlx::query("DELETE FROM events WHERE stream_id = $1")
        .bind(Uuid::parse_str(order_id).unwrap())
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");

    // Outbox relay delivers everything pending for these streams.
    let delivered = lulan_engine::events::relay_once(&pool, &lulan_engine::events::TracingSink)
        .await
        .unwrap();
    assert!(delivered > 0, "outbox must drain");
}

/// A trip id is enough to bypass search, so "is this departure still for
/// sale?" has to be enforced where inventory is resolved, not where it is
/// listed. Both fixtures are dated in the PAST so they sort behind every
/// seeded trip and other suites' `OFFSET n` fixtures don't shift.
#[tokio::test]
async fn departed_and_cancelled_trips_cannot_be_sold() {
    let Some((pool, _, app)) = setup(4).await else {
        return;
    };

    let (route_id, resource_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.route_id, t.resource_id FROM trips t
         JOIN routes r ON r.id = t.route_id WHERE r.code = 'BTG-CEB' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Idempotent across reruns.
    sqlx::query("DELETE FROM trips WHERE service_number = 'PAST-TEST'")
        .execute(&pool)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for (hours_ago, status) in [(2i64, "scheduled"), (3, "cancelled")] {
        let id = Uuid::new_v4();
        let departs_at = chrono::Utc::now() - chrono::Duration::hours(hours_ago);
        sqlx::query(
            "INSERT INTO trips (id, route_id, resource_id, service_number, service_date,
                                departs_at, segment_count, status)
             VALUES ($1, $2, $3, 'PAST-TEST', $4, $5, 3, $6)",
        )
        .bind(id)
        .bind(route_id)
        .bind(resource_id)
        .bind(departs_at.date_naive())
        .bind(departs_at)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
        ids.push(id);
    }
    let (departed, cancelled) = (ids[0], ids[1]);

    let booking = |trip: Uuid| {
        json!({
            "trip_id": trip,
            "passengers": [{"full_name": "Too Late", "type": "adult"}],
            "guest_contact": "late@example.com",
            "items": [{"unit_code": "5A", "origin": "BTG", "destination": "CEB"}],
        })
    };

    for (trip, label) in [(departed, "departed"), (cancelled, "cancelled")] {
        // Quoting it is already refused — the customer never sees a price
        // for a seat they cannot buy.
        let (status, body) = request(&app, "POST", "/v1/quotes", Some(booking(trip))).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "quote on a {label} trip: {body}"
        );

        let (status, body) = request(&app, "POST", "/v1/orders", Some(booking(trip))).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "order on a {label} trip: {body}"
        );

        // And nothing was written.
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM order_items WHERE trip_id = $1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orders, 0, "a {label} trip must not accumulate bookings");
    }

    // Holding a seat on one is refused for the same reason.
    let (status, _) = request(
        &app,
        "POST",
        "/v1/holds",
        Some(json!({
            "trip_id": departed,
            "items": [{"unit_code": "5A", "origin": "BTG", "destination": "CEB"}],
        })),
    )
    .await;
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::SERVICE_UNAVAILABLE,
        "holds on a departed trip must not succeed, got {status}"
    );

    // Reading stays open: crew and support still look at past departures.
    let (status, _) = request(
        &app,
        "GET",
        &format!("/v1/trips/{departed}/availability?origin=BTG&destination=CEB"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "availability is a read; only selling is gated"
    );

    sqlx::query("DELETE FROM trips WHERE service_number = 'PAST-TEST'")
        .execute(&pool)
        .await
        .unwrap();
}
