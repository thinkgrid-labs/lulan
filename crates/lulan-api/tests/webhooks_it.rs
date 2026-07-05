//! Webhook integration (Phase 6): admin-gated registration, HMAC-signed
//! deliveries to a real in-process receiver, event-type filtering, and
//! retry-with-backoff on failure. Requires TEST_DATABASE_URL.
//!
//! Shares the offset-2 trip with quotes_it (which wipes its own fixtures
//! at start and runs earlier); this test touches only seats 9C/9D.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use lulan_engine::webhooks::{WebhookSink, deliver_due, verify_signature};
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_KEY: &str = "llk_test_admin_key_webhooks_it";

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

/// A live HTTP receiver capturing (signature header, body) pairs.
async fn spawn_receiver() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let app = axum::Router::new()
        .route(
            "/hook",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let captured = captured.clone();
                async move {
                    let signature = headers
                        .get("x-lulan-signature")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    captured.lock().await.push((signature, body));
                    "ok"
                }
            }),
        )
        .route(
            "/fail",
            axum::routing::post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), received)
}

#[tokio::test]
async fn webhooks_deliver_signed_filtered_events_with_retries() {
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
    lulan_api::seed::seed(&pool).await.unwrap();
    lulan_api::auth::bootstrap_admin_key(&pool, ADMIN_KEY)
        .await
        .unwrap();

    // Idempotent fixture: retire stale endpoints, drain the outbox
    // backlog from other suites, free our seats.
    sqlx::query("UPDATE webhook_endpoints SET active = false")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE outbox SET delivered_at = now() WHERE delivered_at IS NULL")
        .execute(&pool)
        .await
        .unwrap();
    let trip_id: Uuid =
        sqlx::query("SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id WHERE r.code = 'BTG-CEB' ORDER BY t.departs_at DESC LIMIT 1 OFFSET 2")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 0
         FROM capacity_units cu
         WHERE so.trip_id = $1 AND cu.id = so.unit_id AND cu.code IN ('9C','9D')",
    )
    .bind(trip_id)
    .execute(&pool)
    .await
    .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);
    let (receiver_url, received) = spawn_receiver().await;

    // Registration is admin-gated and audited.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/webhooks",
        Some(json!({"url": "http://x"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, endpoint) = call(
        &app,
        "POST",
        "/v1/webhooks",
        Some(json!({
            "url": format!("{receiver_url}/hook"),
            "event_types": ["order_created", "payment_captured"],
        })),
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{endpoint}");
    let secret = endpoint["secret"].as_str().unwrap().to_string();

    // A failing endpoint subscribed to everything, to exercise retries.
    let (_, failing) = call(
        &app,
        "POST",
        "/v1/webhooks",
        Some(json!({"url": format!("{receiver_url}/fail")})),
        Some(ADMIN_KEY),
    )
    .await;
    let failing_id = failing["id"].as_str().unwrap();

    // Book a guest order → order_created + inventory_locked events.
    let (status, order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id,
            "passengers": [{"full_name": "Hook Test", "type": "adult"}],
            "guest_contact": "hook@example.com",
            "items": [{"unit_code": "9C", "origin": "BTG", "destination": "CEB"}],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let order_id = order["order_id"].as_str().unwrap();

    // Drive the pipeline deterministically: relay fans out, worker POSTs.
    let sink = WebhookSink::new(pool.clone());
    while lulan_engine::events::relay_once(&pool, &sink)
        .await
        .unwrap()
        > 0
    {}
    let client = reqwest::Client::new();
    let stats = deliver_due(&pool, &client).await.unwrap();
    assert!(stats.delivered >= 1, "filtered endpoint gets order_created");
    assert!(stats.retried >= 1, "failing endpoint scheduled for retry");

    // The receiver got exactly the filtered event, correctly signed.
    let inbox = received.lock().await.clone();
    let ours: Vec<&(String, String)> = inbox
        .iter()
        .filter(|(_, body)| body.contains(order_id))
        .collect();
    assert_eq!(ours.len(), 1, "only order_created passes the filter");
    let (signature, body) = ours[0];
    assert!(verify_signature(&secret, signature, body), "HMAC verifies");
    assert!(!verify_signature("whsec_wrong", signature, body));
    let event: Value = serde_json::from_str(body).unwrap();
    assert_eq!(event["event_type"], "order_created");
    assert_eq!(event["payload"]["passengers"][0]["full_name"], "Hook Test");

    // Retry machinery: the failing delivery is pending with backoff…
    let row = sqlx::query(
        "SELECT status, attempts FROM webhook_deliveries
         WHERE endpoint_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(Uuid::parse_str(failing_id).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "pending");
    assert_eq!(row.get::<i32, _>("attempts"), 1);

    // …not due yet (backoff), so a second pass does nothing…
    let stats = deliver_due(&pool, &client).await.unwrap();
    assert_eq!(stats.delivered + stats.retried + stats.exhausted, 0);

    // …until its time comes (backdated here), then attempts increment.
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() WHERE endpoint_id = $1")
        .bind(Uuid::parse_str(failing_id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    deliver_due(&pool, &client).await.unwrap();
    let attempts: i32 = sqlx::query_scalar(
        "SELECT attempts FROM webhook_deliveries WHERE endpoint_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(Uuid::parse_str(failing_id).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, 2);

    // Deactivation stops future deliveries and is audited.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/webhooks/{failing_id}"),
        None,
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action IN ('webhook.created', 'webhook.deactivated')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(audits >= 3, "admin mutations are audited");
}
