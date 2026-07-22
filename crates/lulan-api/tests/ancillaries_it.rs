//! Ancillaries: catalog browsing, quote → order price locking for
//! add-ons, per-passenger/per-leg validation, and admin gating.
//! Requires TEST_DATABASE_URL. Uses FUTURE return-route trips and row-11
//! seats (11D — quotes_it touches 11A/11B on the outbound route only).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_KEY: &str = "llk_test_admin_key_ancillaries_it";

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

async fn future_return_trips(pool: &PgPool) -> Vec<Uuid> {
    sqlx::query(
        "SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id
         WHERE r.code = 'CEB-BTG' AND t.departs_at > now() ORDER BY t.departs_at",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get(0))
    .collect()
}

#[tokio::test]
async fn ancillaries_price_into_quotes_and_orders() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("lulan_api=debug")
        .try_init();
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

    let returns = future_return_trips(&pool).await;
    let trip = returns[returns.len() - 1]; // furthest-out return trip
    // Idempotent fixture: free 11D and drop this suite's stale orders.
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT oi.order_id FROM order_items oi
         JOIN capacity_units cu ON cu.id = oi.unit_id
         WHERE oi.trip_id = $1 AND cu.code = '11D'",
    )
    .bind(trip)
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
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 0
         FROM capacity_units cu
         WHERE cu.id = so.unit_id AND so.trip_id = $1 AND cu.code = '11D'",
    )
    .bind(trip)
    .execute(&pool)
    .await
    .unwrap();
    // This suite creates LOUNGE below; purge leftovers from prior runs.
    sqlx::query("DELETE FROM ancillaries WHERE code = 'LOUNGE'")
        .execute(&pool)
        .await
        .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);

    // ---- Catalog is public and seeded --------------------------------
    let (status, catalog) = call(&app, "GET", "/v1/ancillaries", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let codes: Vec<&str> = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["code"].as_str().unwrap())
        .collect();
    for code in ["BAG20", "MEAL_STD", "INSURE", "PRIORITY"] {
        assert!(codes.contains(&code), "seeded catalog has {code}");
    }
    let meal_price = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "MEAL_STD")
        .unwrap()["price_minor"]
        .as_i64()
        .unwrap();

    // Admin surface is gated.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/ancillaries",
        Some(json!({"code": "X", "name": "X", "kind": "x", "price_minor": 1})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // ---- Quote with add-ons: meal (per-passenger, per-leg) + insurance
    //      (per-passenger, itinerary) — totals include them ------------
    let base_quote_body = json!({
        "trip_id": trip,
        "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG", "passenger_type": "adult"}],
    });
    let (_, bare) = call(&app, "POST", "/v1/quotes", Some(base_quote_body), None).await;
    let bare_total = bare["total_minor"].as_i64().unwrap();

    let quote_body = json!({
        "trip_id": trip,
        "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG", "passenger_type": "adult"}],
        "ancillaries": [
            {"code": "MEAL_STD", "trip_id": trip, "passenger": 0},
            {"code": "INSURE", "passenger": 0},
        ],
    });
    let (status, quote) = call(&app, "POST", "/v1/quotes", Some(quote_body), None).await;
    assert_eq!(status, StatusCode::OK, "{quote}");
    let quoted_total = quote["total_minor"].as_i64().unwrap();
    let addon_total: i64 = quote["ancillaries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["total_minor"].as_i64().unwrap())
        .sum();
    assert_eq!(quoted_total, bare_total + addon_total);
    assert_eq!(addon_total, meal_price + 15_000);
    let token = quote["quote_token"].as_str().unwrap().to_string();

    // Validation: journey-scoped without trip_id, and foreign trips, fail.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip,
            "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG"}],
            "ancillaries": [{"code": "MEAL_STD", "passenger": 0}],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip,
            "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG"}],
            "ancillaries": [{"code": "MEAL_STD", "trip_id": Uuid::new_v4(), "passenger": 0}],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // ---- Order at the quoted price: same lines required --------------
    // Dropping a quoted add-on is a mismatch (protects the locked total).
    let (status, _) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip,
            "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG"}],
            "passengers": [{"full_name": "Addon Test", "type": "adult"}],
            "guest_contact": "addon@example.com",
            "quote_token": token,
            "ancillaries": [{"code": "MEAL_STD", "trip_id": trip, "passenger": 0}],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing quoted add-on");

    let (status, order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": trip,
            "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG"}],
            "passengers": [{"full_name": "Addon Test", "type": "adult"}],
            "guest_contact": "addon@example.com",
            "quote_token": token,
            "ancillaries": [
                {"code": "MEAL_STD", "trip_id": trip, "passenger": 0},
                {"code": "INSURE", "passenger": 0},
            ],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["total_minor"].as_i64().unwrap(), quoted_total);
    let lines = order["ancillaries"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|l| l["passenger_id"].is_string() && l["total_minor"].as_i64().unwrap() > 0)
    );
    let meal = lines.iter().find(|l| l["code"] == "MEAL_STD").unwrap();
    assert_eq!(meal["trip_id"].as_str().unwrap(), trip.to_string());

    // Payment intent covers fare + add-ons (total flows through).
    let order_id = order["order_id"].as_str().unwrap();
    let (_, payment) = call(
        &app,
        "POST",
        &format!("/v1/orders/{order_id}/payment"),
        Some(json!({})),
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(payment["order_id"].as_str().unwrap(), order_id);

    // ---- Admin lifecycle: create then deactivate; catalog reflects it -
    let (status, created) = call(
        &app,
        "POST",
        "/v1/ancillaries",
        Some(json!({
            "code": "LOUNGE", "name": "Lounge access", "kind": "service",
            "price_minor": 50_000, "per": "passenger", "scope": "journey",
        })),
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let lounge_id = created["id"].as_str().unwrap();
    // Duplicate codes are a clean conflict, not a 500.
    let (status, dup) = call(
        &app,
        "POST",
        "/v1/ancillaries",
        Some(json!({"code": "LOUNGE", "name": "Again", "kind": "service", "price_minor": 1})),
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{dup}");
    let (_, catalog) = call(&app, "GET", "/v1/ancillaries", None, None).await;
    assert!(
        catalog
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["code"] == "LOUNGE")
    );
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/ancillaries/{lounge_id}"),
        None,
        Some(ADMIN_KEY),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, catalog) = call(&app, "GET", "/v1/ancillaries", None, None).await;
    assert!(
        !catalog
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["code"] == "LOUNGE"),
        "deactivated add-ons leave the catalog"
    );
    // …and can no longer be quoted.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/quotes",
        Some(json!({
            "trip_id": trip,
            "items": [{"unit_code": "11D", "origin": "CEB", "destination": "BTG"}],
            "ancillaries": [{"code": "LOUNGE", "trip_id": trip, "passenger": 0}],
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
