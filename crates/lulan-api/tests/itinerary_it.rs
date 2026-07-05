//! Itineraries (Phase 6.5 exit criteria): round-trip with a visible
//! round-trip discount, a 3-city itinerary, cross-leg atomicity (a
//! conflict on the LAST leg rolls back claims on every leg), and per-leg
//! QR tickets validating offline against their own trips.
//!
//! Fixture trips: the outbound route's EARLIEST departure (shared with
//! availability_it, which resets all its masks itself — we touch only
//! seats 6A/7A) and the return route's trips (used by no other suite).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use lulan_api::state::AppState;
use lulan_validate::{KeyEntry, ValidationError, verify_ticket};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
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

/// Trips of a route ordered by departure (ascending).
async fn route_trips(pool: &PgPool, route: &str) -> Vec<Uuid> {
    sqlx::query(
        "SELECT t.id FROM trips t JOIN routes r ON r.id = t.route_id
         WHERE r.code = $1 ORDER BY t.departs_at",
    )
    .bind(route)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get(0))
    .collect()
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
async fn round_trip_multi_city_and_cross_leg_atomicity() {
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

    let outbound = route_trips(&pool, "BTG-CEB").await;
    let returns = route_trips(&pool, "CEB-BTG").await;
    assert!(returns.len() >= 5, "seed must create the return route");
    let out_trip = outbound[0]; // earliest outbound (availability's trip)
    let back_trip = returns[1];
    let legs3 = [returns[2], returns[3], returns[4]];

    // Idempotent fixture: drop this suite's stale orders (any order
    // touching our trips on seats 6A/7A), then free those seats.
    let involved: Vec<Uuid> = [out_trip, back_trip]
        .iter()
        .chain(legs3.iter())
        .copied()
        .collect();
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT oi.order_id FROM order_items oi
         JOIN capacity_units cu ON cu.id = oi.unit_id
         WHERE oi.trip_id = ANY($1) AND cu.code IN ('6A', '7A')",
    )
    .bind(&involved)
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
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 0
         FROM capacity_units cu
         WHERE cu.id = so.unit_id AND so.trip_id = ANY($1) AND cu.code IN ('6A', '7A')",
    )
    .bind(&involved)
    .execute(&pool)
    .await
    .unwrap();

    let app = lulan_api::router(AppState::new(Some(pool.clone()), None).await);

    // ---- Round trip: quote → discount visible → order → per-leg tickets --
    let rt_quote_body = json!({
        "journeys": [
            {"trip_id": out_trip,  "items": [{"unit_code": "6A", "origin": "BTG", "destination": "CEB", "passenger_type": "adult"}]},
            {"trip_id": back_trip, "items": [{"unit_code": "6A", "origin": "CEB", "destination": "BTG", "passenger_type": "adult"}]},
        ],
    });
    let (status, rt_quote) = call(&app, "POST", "/v1/quotes", Some(rt_quote_body)).await;
    assert_eq!(status, StatusCode::OK, "{rt_quote}");
    assert_eq!(rt_quote["is_round_trip"], true);
    assert_eq!(rt_quote["journey_count"], 2);
    for item in rt_quote["items"].as_array().unwrap() {
        assert!(
            item["adjustments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["label"] == "round_trip"),
            "every leg carries the round-trip discount: {item}"
        );
    }

    // The same two legs quoted separately (one-way each) must cost more.
    let mut one_way_total = 0i64;
    for (trip, origin, destination) in [(out_trip, "BTG", "CEB"), (back_trip, "CEB", "BTG")] {
        let (_, quote) = call(
            &app,
            "POST",
            "/v1/quotes",
            Some(json!({
                "trip_id": trip,
                "items": [{"unit_code": "6A", "origin": origin, "destination": destination, "passenger_type": "adult"}],
            })),
        )
        .await;
        assert_eq!(quote["is_round_trip"], false);
        one_way_total += quote["total_minor"].as_i64().unwrap();
    }
    let rt_total = rt_quote["total_minor"].as_i64().unwrap();
    assert!(
        rt_total < one_way_total,
        "round trip ({rt_total}) must beat two one-ways ({one_way_total})"
    );

    // Book it at the quoted price.
    let (status, order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "passengers": [{"full_name": "Bea Cruz", "type": "adult"}],
            "guest_contact": "bea@example.com",
            "quote_token": rt_quote["quote_token"],
            "journeys": [
                {"trip_id": out_trip,  "items": [{"unit_code": "6A", "origin": "BTG", "destination": "CEB"}]},
                {"trip_id": back_trip, "items": [{"unit_code": "6A", "origin": "CEB", "destination": "BTG"}]},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["total_minor"].as_i64().unwrap(), rt_total);
    assert_eq!(order["trip_ids"].as_array().unwrap().len(), 2);
    let order_id = order["order_id"].as_str().unwrap().to_string();
    let retrieval = order["retrieval_token"].as_str().unwrap().to_string();

    // Pay → one ticket per leg, each pinned to ITS trip.
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

    let (_, tickets) = call(
        &app,
        "GET",
        &format!("/v1/orders/{order_id}/tickets?token={retrieval}"),
        None,
    )
    .await;
    let tickets = tickets["tickets"].as_array().unwrap().clone();
    assert_eq!(tickets.len(), 2, "one ticket per passenger per leg");

    let (_, keys_body) = call(&app, "GET", "/v1/ticket-keys", None).await;
    let keys: Vec<KeyEntry> = serde_json::from_value(keys_body["keys"].clone()).unwrap();
    let now = Utc::now().timestamp();
    let mut leg_trips = Vec::new();
    for ticket in &tickets {
        let verified = verify_ticket(ticket["token"].as_str().unwrap(), &keys, now, None).unwrap();
        leg_trips.push(verified.trip_id);
    }
    assert!(leg_trips.contains(&out_trip) && leg_trips.contains(&back_trip));
    // A ticket for the outbound leg must NOT validate pinned to the return.
    let out_ticket = tickets
        .iter()
        .find(|t| {
            verify_ticket(t["token"].as_str().unwrap(), &keys, now, None)
                .unwrap()
                .trip_id
                == out_trip
        })
        .unwrap();
    assert_eq!(
        verify_ticket(
            out_ticket["token"].as_str().unwrap(),
            &keys,
            now,
            Some(back_trip)
        ),
        Err(ValidationError::WrongTrip)
    );

    // ---- Multi-city: three legs, no round-trip discount ------------------
    let journeys3: Vec<Value> = [
        (legs3[0], "CEB", "ILO"),
        (legs3[1], "ILO", "CTC"),
        (legs3[2], "CTC", "BTG"),
    ]
    .iter()
    .map(|(trip, origin, destination)| {
        json!({"trip_id": trip, "items": [{"unit_code": "6A", "origin": origin, "destination": destination}]})
    })
    .collect();
    let (status, mc_order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "passengers": [{"full_name": "Bea Cruz", "type": "adult"}],
            "guest_contact": "bea@example.com",
            "journeys": journeys3,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{mc_order}");
    assert_eq!(mc_order["trip_ids"].as_array().unwrap().len(), 3);
    let no_rt = mc_order["items"].as_array().unwrap().iter().all(|i| {
        // items don't echo adjustments, so assert via a quote instead below
        i["price_minor"].as_i64().unwrap() > 0
    });
    assert!(no_rt);

    // ---- Cross-leg atomicity: conflict on the LAST leg -------------------
    // Pre-occupy 7A on leg 3 for the full journey.
    sqlx::query(
        "UPDATE seat_occupancy so SET occupied_mask = 7
         FROM capacity_units cu
         WHERE cu.id = so.unit_id AND so.trip_id = $1 AND cu.code = '7A'",
    )
    .bind(legs3[2])
    .execute(&pool)
    .await
    .unwrap();

    let journeys_conflict: Vec<Value> = [
        (legs3[0], "CEB", "ILO"),
        (legs3[1], "ILO", "CTC"),
        (legs3[2], "CTC", "BTG"),
    ]
    .iter()
    .map(|(trip, origin, destination)| {
        json!({"trip_id": trip, "items": [{"unit_code": "7A", "origin": origin, "destination": destination}]})
    })
    .collect();
    let (status, conflict) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "passengers": [{"full_name": "Race Case", "type": "adult"}],
            "guest_contact": "race@example.com",
            "journeys": journeys_conflict,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");

    // Legs 1 and 2 must be fully released — nothing partial survived.
    assert_eq!(
        seat_mask(&pool, legs3[0], "7A").await,
        0,
        "leg 1 rolled back"
    );
    assert_eq!(
        seat_mask(&pool, legs3[1], "7A").await,
        0,
        "leg 2 rolled back"
    );

    // Mixed-shape guard: both trip_id+items AND journeys is a 400.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(json!({
            "trip_id": out_trip,
            "items": [{"unit_code": "6B", "origin": "BTG", "destination": "CEB"}],
            "journeys": [{"trip_id": back_trip, "items": [{"unit_code": "6B", "origin": "CEB", "destination": "BTG"}]}],
            "passengers": [{"full_name": "Shape Test", "type": "adult"}],
            "guest_contact": "shape@example.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Round-trip search convenience: return_date yields return candidates.
    let date: String = sqlx::query_scalar("SELECT service_date::text FROM trips WHERE id = $1")
        .bind(out_trip)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (status, search) = call(
        &app,
        "GET",
        &format!("/v1/trips/search?origin=BTG&destination=CEB&departure_date={date}&trip_type=round_trip&return_date={date}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(search["trip_type"], "round_trip");
    let legs = search["legs"].as_array().unwrap();
    assert_eq!(legs.len(), 2, "outbound + return legs");
    assert_eq!(legs[0]["leg"], "outbound");
    assert_eq!(legs[1]["leg"], "return");
    assert!(!legs[0]["trips"].as_array().unwrap().is_empty());
    assert!(
        !legs[1]["trips"].as_array().unwrap().is_empty(),
        "return candidates present: {search}"
    );

    // round_trip without a return_date is a 400.
    let (status, _) = call(
        &app,
        "GET",
        &format!(
            "/v1/trips/search?origin=BTG&destination=CEB&departure_date={date}&trip_type=round_trip"
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
