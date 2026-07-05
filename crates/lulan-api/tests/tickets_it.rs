//! Phase 5 exit criterion, end to end: book → pay (fake) → tickets
//! auto-issued → verified OFFLINE by lulan-validate using only the cached
//! key set → boarding scans sync idempotently → order aggregates to
//! Boarded. Requires TEST_DATABASE_URL (skips otherwise).
//!
//! Uses trip offset 1 (shared with claims_it's hold test but touching only
//! seats 13A–13D, which nothing else uses).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use lulan_api::state::AppState;
use lulan_validate::{KeyEntry, ValidationError, verify_ticket};
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
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

#[tokio::test]
async fn offline_ticket_flow_from_booking_to_boarded() {
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

    let trip_id: Uuid =
        sqlx::query("SELECT id FROM trips ORDER BY departs_at DESC LIMIT 1 OFFSET 1")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
    // Free the 13x row used by this test (idempotent across runs).
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 0
         FROM capacity_units cu
         WHERE so.trip_id = $1 AND cu.id = so.unit_id AND cu.code LIKE '13%'",
    )
    .bind(trip_id)
    .execute(&pool)
    .await
    .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);

    // Book two passengers.
    let (status, order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip_id,
            "passengers": [
                {"full_name": "Ana Reyes", "type": "adult"},
                {"full_name": "Lola Remedios", "type": "senior", "birthdate": "1950-01-02"},
            ],
            "items": [
                {"unit_code": "13A", "origin": "BTG", "destination": "CEB", "passenger": 0},
                {"unit_code": "13B", "origin": "BTG", "destination": "CEB", "passenger": 1},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let order_id = order["order_id"].as_str().unwrap().to_string();

    // Tickets before payment must be refused.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/tickets"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "no tickets before payment");

    // Pay: webhook capture auto-issues tickets.
    let (_, payment) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/payment"),
        Some(json!({})),
    )
    .await;
    let intent = payment["payment_intent_id"].as_str().unwrap();
    let (_, hook) = call(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
    )
    .await;
    assert_eq!(hook["order_status"], "ticketed");

    // Fetch tickets; issuing again is idempotent (same 2 tickets).
    let (status, tickets) =
        call(&app, "GET", &format!("/v1/orders/{order_id}/tickets"), None).await;
    assert_eq!(status, StatusCode::OK);
    let tickets = tickets["tickets"].as_array().unwrap().clone();
    assert_eq!(tickets.len(), 2, "one ticket per passenger");
    let (_, reissued) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/tickets"),
        Some(json!({})),
    )
    .await;
    assert_eq!(reissued["tickets"].as_array().unwrap().len(), 2);

    // ---- OFFLINE from here: cache the key set, then no server calls ----
    let (status, keys_body) = call(&app, "GET", "/v1/ticket-keys", None).await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<KeyEntry> = serde_json::from_value(keys_body["keys"].clone()).unwrap();
    assert!(!keys.is_empty());

    let now = Utc::now().timestamp();
    let mut verified_ids = Vec::new();
    for ticket in &tickets {
        let token = ticket["token"].as_str().unwrap();
        let verified = verify_ticket(token, &keys, now, Some(trip_id)).expect("offline verify");
        assert_eq!(
            verified.ticket_id.to_string(),
            ticket["ticket_id"].as_str().unwrap()
        );
        assert_eq!((verified.from_index, verified.to_index), (0, 3));
        verified_ids.push(verified.ticket_id);
    }
    // The passenger names on the QR match the manifest.
    let names: Vec<String> = tickets
        .iter()
        .map(|t| {
            verify_ticket(t["token"].as_str().unwrap(), &keys, now, None)
                .unwrap()
                .passenger_name
        })
        .collect();
    assert!(names.contains(&"Ana Reyes".to_string()));
    assert!(names.contains(&"Lola Remedios".to_string()));

    // Tampered token fails offline; wrong-trip pin fails offline.
    let token = tickets[0]["token"].as_str().unwrap();
    let mut tampered = token.to_string();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert!(matches!(
        verify_ticket(&tampered, &keys, now, None),
        Err(ValidationError::BadSignature | ValidationError::Malformed)
    ));
    assert_eq!(
        verify_ticket(token, &keys, now, Some(Uuid::new_v4())),
        Err(ValidationError::WrongTrip)
    );

    // ---- Back online: sync the boarding journal (with a replayed row) ----
    let scanned_at = Utc::now();
    let scans = json!({
        "device_id": "gate-1",
        "scans": [
            {"ticket_id": verified_ids[0], "scanned_at": scanned_at},
            {"ticket_id": verified_ids[0], "scanned_at": scanned_at}, // journal replay
            {"ticket_id": verified_ids[1], "scanned_at": scanned_at},
        ],
    });
    let (status, sync) = call(&app, "POST", "/v1/scans", Some(scans.clone())).await;
    assert_eq!(status, StatusCode::OK, "{sync}");
    let outcomes = sync["outcomes"].as_array().unwrap();
    assert_eq!(outcomes[0]["status"], "boarded");
    assert_eq!(outcomes[1]["status"], "duplicate_scan");
    assert_eq!(outcomes[2]["status"], "boarded");
    assert_eq!(
        outcomes[2]["order_status"], "boarded",
        "last passenger aboard aggregates the order"
    );

    // Whole-batch replay (device re-uploads its journal): all no-ops.
    let (_, resync) = call(&app, "POST", "/v1/scans", Some(scans)).await;
    for outcome in resync["outcomes"].as_array().unwrap() {
        assert_eq!(outcome["status"], "duplicate_scan");
    }

    // Cloned-QR case: a DIFFERENT device scans an already-boarded ticket —
    // recorded as evidence, flagged as already boarded.
    let (_, clone_scan) = call(
        &app,
        "POST",
        "/v1/scans",
        Some(json!({
            "device_id": "gate-2",
            "scans": [{"ticket_id": verified_ids[0], "scanned_at": Utc::now()}],
        })),
    )
    .await;
    assert_eq!(clone_scan["outcomes"][0]["status"], "already_boarded");

    // Order read model + replayed stream both say Boarded.
    let (_, details) = call(&app, "GET", &format!("/v1/orders/{order_id}"), None).await;
    assert_eq!(details["status"], "boarded");
    let store = lulan_engine::orders::OrderStore::new(pool.clone());
    assert_eq!(
        store
            .replay_status(Uuid::parse_str(&order_id).unwrap())
            .await
            .unwrap()
            .unwrap(),
        lulan_engine::domain::OrderStatus::Boarded
    );

    // Unknown ticket ids don't poison a batch.
    let (_, unknown) = call(
        &app,
        "POST",
        "/v1/scans",
        Some(json!({
            "device_id": "gate-1",
            "scans": [{"ticket_id": Uuid::new_v4(), "scanned_at": Utc::now()}],
        })),
    )
    .await;
    assert_eq!(unknown["outcomes"][0]["status"], "unknown_ticket");
}
