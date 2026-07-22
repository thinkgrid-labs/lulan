//! Phase 7.5 exit criteria, as one scripted flow with only IdP tokens:
//! an admin enrols staff; an ops staffer builds a route + schedule and
//! publishes fare rules (with rollback); a support staffer reads the
//! manifest and refunds an order (tickets void, seats free, audit rows
//! name the human); a non-enrolled JWT gets 403 across /v1/admin/*; role
//! boundaries hold (ops ≠ support). Requires TEST_DATABASE_URL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

const ISSUER: &str = "https://idp.admin-test";
const IDP_SECRET: &str = "admin-it-shared-secret";
/// Machine credential for the storefront/provider paths this flow drives
/// (payment intent + capture). Sent as `Authorization: Bearer llk_…`,
/// which both the integration extractor and order access accept.
const API_KEY: &str = "llk_test_admin_it_key";

fn jwt(subject: &str) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({"iss": ISSUER, "sub": subject, "exp": Utc::now().timestamp() + 3600}),
        &jsonwebtoken::EncodingKey::from_secret(IDP_SECRET.as_bytes()),
    )
    .unwrap()
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
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

async fn seat_mask(pool: &PgPool, trip: Uuid, code: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT so.occupied_mask FROM seat_occupancy so
         JOIN capacity_units cu ON cu.id = so.unit_id
         WHERE so.trip_id = $1 AND cu.code = $2",
    )
    .bind(trip)
    .bind(code)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn admin_operations_run_the_business_with_only_idp_tokens() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    unsafe {
        std::env::set_var("LULAN_IDP_ISSUER", ISSUER);
        std::env::set_var("LULAN_IDP_HS256_SECRET", IDP_SECRET);
    }
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .unwrap();
    lulan_api::MIGRATOR.run(&pool).await.unwrap();
    lulan_api::seed::seed(&pool).await.unwrap();
    lulan_api::staff::bootstrap_admin_staff(&pool, &format!("{ISSUER}|boss"))
        .await
        .unwrap();
    lulan_api::auth::bootstrap_admin_key(&pool, API_KEY)
        .await
        .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);
    let boss = jwt("boss");
    let stranger = jwt("nobody");
    // Unique-per-run codes: creations are append-only across reruns.
    let run = &Uuid::new_v4().simple().to_string()[..6].to_uppercase();

    // ---- Non-enrolled JWT: 403 on EVERY admin path ----------------------
    let zero = "00000000-0000-0000-0000-000000000000";
    for (method, path) in [
        ("POST", "/v1/admin/staff".to_string()),
        ("GET", "/v1/admin/staff".to_string()),
        ("DELETE", format!("/v1/admin/staff/{zero}")),
        ("POST", "/v1/admin/fare-rules".to_string()),
        ("GET", "/v1/admin/fare-rules".to_string()),
        ("POST", format!("/v1/admin/fare-rules/{zero}/activate")),
        ("POST", "/v1/admin/locations".to_string()),
        ("POST", "/v1/admin/routes".to_string()),
        ("POST", "/v1/admin/vessels".to_string()),
        ("POST", "/v1/admin/trips".to_string()),
        ("POST", format!("/v1/admin/trips/{zero}/cancel")),
        ("GET", format!("/v1/admin/trips/{zero}/manifest")),
        ("GET", "/v1/admin/orders?name=x".to_string()),
        ("POST", format!("/v1/admin/orders/{zero}/refund")),
    ] {
        let (status, _) = call(&app, method, &path, Some(json!({})), Some(&stranger)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} must 403 for non-staff"
        );
    }

    // ---- Admin enrols ops + support (audited with staff_id) -------------
    let (status, ops_rec) = call(
        &app,
        "POST",
        "/v1/admin/staff",
        Some(json!({"subject": "opal", "display_name": "Opal Ops", "role": "ops"})),
        Some(&boss),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ops_rec}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/admin/staff",
        Some(json!({"subject": "sunny", "display_name": "Sunny Support", "role": "support"})),
        Some(&boss),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let enrolled_by_boss: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log a JOIN staff s ON s.id = a.staff_id
         WHERE a.action = 'staff.enrolled' AND s.subject = 'boss'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(enrolled_by_boss >= 2, "audit names the enrolling human");

    let opal = jwt("opal");
    let sunny = jwt("sunny");

    // ---- Ops builds a network: locations → route → vessel → trips -------
    for (code, name) in [
        ("AAA", "Alpha Port"),
        ("BBB", "Beta Port"),
        ("CCC", "Gamma Port"),
    ] {
        let (status, body) = call(
            &app,
            "POST",
            "/v1/admin/locations",
            Some(json!({"code": format!("{code}{run}"), "name": name})),
            Some(&opal),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = call(
        &app,
        "POST",
        "/v1/admin/routes",
        Some(json!({
            "code": format!("AX-{run}"),
            "name": "Alpha Express",
            "stops": [
                {"location_code": format!("AAA{run}"), "arrive_offset_min": 0, "depart_offset_min": 0},
                {"location_code": format!("BBB{run}"), "arrive_offset_min": 60, "depart_offset_min": 70},
                {"location_code": format!("CCC{run}"), "arrive_offset_min": 150, "depart_offset_min": 150},
            ],
        })),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = call(
        &app,
        "POST",
        "/v1/admin/vessels",
        Some(json!({
            "code": format!("MV-{run}"), "name": "MV Test", "kind": "ferry",
            "seats": [
                {"code": "1A", "fare_class": "economy"},
                {"code": "1B", "fare_class": "economy"},
                {"code": "1C", "fare_class": "economy"},
                {"code": "1D", "fare_class": "economy"},
            ],
            "pools": [{"code": "CARGO_KG", "capacity": 500}],
        })),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let dep1 = (Utc::now() + Duration::days(3)).to_rfc3339();
    let dep2 = (Utc::now() + Duration::days(4)).to_rfc3339();
    let (status, created) = call(
        &app,
        "POST",
        "/v1/admin/trips",
        Some(json!({
            "route_code": format!("AX-{run}"),
            "vessel_code": format!("MV-{run}"),
            "operator_code": "LUL",
            "service_number": "AX 100",
            "departures": [dep1, dep2],
        })),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let trip_ids: Vec<Uuid> = created["trip_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| Uuid::parse_str(v.as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(trip_ids.len(), 2);

    // Support cannot touch ops surface.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/admin/locations",
        Some(json!({"code": "NOPE", "name": "n"})),
        Some(&sunny),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "support ≠ ops");

    // The new network is live on the public API, schedule included.
    let date = (Utc::now() + Duration::days(3)).date_naive();
    let (status, search) = call(
        &app,
        "GET",
        &format!("/v1/trips/search?origin=AAA{run}&destination=CCC{run}&departure_date={date}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hit = &search["legs"][0]["trips"][0];
    assert_eq!(hit["service_number"], "AX 100");
    assert_eq!(hit["duration_minutes"], 150);

    // ---- Fare publishing with rollback -----------------------------------
    let (_, before) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip_ids[0],
            "items": [{"unit_code": "1A", "origin": format!("AAA{run}"), "destination": format!("CCC{run}")}],
        })),
        None,
    )
    .await;
    let price_before = before["total_minor"].as_i64().unwrap();

    let (_, rules_list) = call(&app, "GET", "/v1/admin/fare-rules", None, Some(&opal)).await;
    // Roll back to the ACTIVE ruleset — not merely the newest row; prior
    // runs leave inactive published rulesets behind.
    let previous_id = rules_list["rulesets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["active"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Publish a doubled economy fare (support is denied; garbage rejected).
    let (status, _) = call(
        &app,
        "POST",
        "/v1/admin/fare-rules",
        Some(json!({})),
        Some(&sunny),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/admin/fare-rules",
        Some(json!({"currency": "PHP", "base_fare_per_segment": {}})),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty ruleset rejected");
    let (status, published) = call(
        &app,
        "POST",
        "/v1/admin/fare-rules",
        Some(json!({
            "currency": "PHP",
            "base_fare_per_segment": {"economy": 30_000, "business": 60_000,
                                       "VEHICLE_DECK": 100_000, "CARGO_KG": 500},
        })),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{published}");

    let (_, after) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip_ids[0],
            "items": [{"unit_code": "1A", "origin": format!("AAA{run}"), "destination": format!("CCC{run}")}],
        })),
        None,
    )
    .await;
    assert!(
        after["total_minor"].as_i64().unwrap() > price_before,
        "published rules price immediately"
    );

    // Rollback to the previous ruleset.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/admin/fare-rules/{previous_id}/activate"),
        Some(json!({})),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, reverted) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip_ids[0],
            "items": [{"unit_code": "1A", "origin": format!("AAA{run}"), "destination": format!("CCC{run}")}],
        })),
        None,
    )
    .await;
    assert_eq!(reverted["total_minor"].as_i64().unwrap(), price_before);

    // ---- Book + pay on the new trip, then support refunds ----------------
    let book = |trip: Uuid, seat: &str| {
        json!({
            "trip_id": trip,
            "items": [{"unit_code": seat, "origin": format!("AAA{run}"), "destination": format!("CCC{run}")}],
            "passengers": [{"full_name": "Refund Case", "type": "adult"}],
            "guest_contact": "refund@example.com",
        })
    };
    let (status, order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(book(trip_ids[0], "1A")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let order_id = order["order_id"].as_str().unwrap().to_string();
    let (_, payment) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/payment"),
        Some(json!({})),
        Some(API_KEY),
    )
    .await;
    let intent = payment["payment_intent_id"].as_str().unwrap();
    let (_, hook) = call(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
        Some(API_KEY),
    )
    .await;
    assert_eq!(hook["order_status"], "ticketed");

    // Manifest shows the passenger with an issued ticket.
    let (status, manifest) = call(
        &app,
        "GET",
        &format!("/v1/admin/trips/{}/manifest", trip_ids[0]),
        None,
        Some(&sunny),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entry = &manifest["seats"][0];
    assert_eq!(entry["passenger"], "Refund Case");
    assert_eq!(entry["ticket_status"], "issued");

    // Order search by contact finds it.
    let (_, found) = call(
        &app,
        "GET",
        "/v1/admin/orders?contact=refund@example.com",
        None,
        Some(&sunny),
    )
    .await;
    assert!(
        found["orders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["order_id"] == order_id.as_str()),
        "{found}"
    );

    // Ops cannot refund (role boundary), support can.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/admin/orders/{order_id}/refund"),
        Some(json!({})),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "ops ≠ support");
    assert_ne!(seat_mask(&pool, trip_ids[0], "1A").await, 0, "seat sold");
    let (status, refunded) = call(
        &app,
        "POST",
        &format!("/v1/admin/orders/{order_id}/refund"),
        Some(json!({})),
        Some(&sunny),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refunded}");
    assert_eq!(refunded["status"], "refunded");
    // Seats released, tickets void, audit names Sunny.
    assert_eq!(seat_mask(&pool, trip_ids[0], "1A").await, 0, "seat freed");
    let voided: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tickets WHERE order_id = $1 AND status = 'void'")
            .bind(Uuid::parse_str(&order_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(voided, 1);
    let by_sunny: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log a JOIN staff s ON s.id = a.staff_id
         WHERE a.action = 'order.refunded' AND s.subject = 'sunny'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(by_sunny >= 1, "refund audit names the human");
    // Double refund is a clean conflict.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/admin/orders/{order_id}/refund"),
        Some(json!({})),
        Some(&sunny),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // ---- Trip cancellation cascades ---------------------------------------
    let (status, order2) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(book(trip_ids[1], "1B")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let order2_id = order2["order_id"].as_str().unwrap().to_string();
    let (_, payment) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order2_id}/payment"),
        Some(json!({})),
        Some(API_KEY),
    )
    .await;
    let intent = payment["payment_intent_id"].as_str().unwrap();
    call(
        &app,
        "POST",
        "/v1/payments/fake/webhook",
        Some(json!({"payment_intent_id": intent, "status": "succeeded"})),
        Some(API_KEY),
    )
    .await;

    let (status, cascade) = call(
        &app,
        "POST",
        &format!("/v1/admin/trips/{}/cancel", trip_ids[1]),
        Some(json!({})),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cascade}");
    assert_eq!(cascade["orders_refunded"], 1);
    assert_eq!(cascade["failures"], 0);
    // Cancelled trips vanish from public search.
    let date2 = (Utc::now() + Duration::days(4)).date_naive();
    let (_, search) = call(
        &app,
        "GET",
        &format!("/v1/trips/search?origin=AAA{run}&destination=CCC{run}&departure_date={date2}"),
        None,
        None,
    )
    .await;
    assert!(
        search["legs"][0]["trips"].as_array().unwrap().is_empty(),
        "cancelled trip is not sold: {search}"
    );

    // Staff revocation locks the door.
    let ops_id = ops_rec["id"].as_str().unwrap();
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/admin/staff/{ops_id}"),
        None,
        Some(&boss),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "POST",
        "/v1/admin/locations",
        Some(json!({"code": "ZZZ", "name": "z"})),
        Some(&opal),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "revoked staff is out");
}
