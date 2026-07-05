//! Auth & identity integration (Phase 6): role-gated admin surface,
//! customer bearer tokens vs guest checkout, retrieval tokens, order
//! claiming, idempotent booking retries, and the rate limiter.
//!
//! Single test fn: it mutates process env (IdP config, rate limit), so
//! everything runs on one timeline. Uses the offset-2 trip, seats
//! 12A/12B/12C only (quotes_it wipes that trip's orders at ITS start and
//! runs before this binary alphabetically… no: auth_it runs first — each
//! suite cleans its own fixtures, so ordering is irrelevant).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use lulan_api::state::AppState;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_KEY: &str = "llk_test_admin_key_auth_it";
const ISSUER: &str = "https://idp.test";
const IDP_SECRET: &str = "auth-it-shared-secret";

#[derive(Default)]
struct Call<'a> {
    api_key: Option<&'a str>,
    bearer: Option<&'a str>,
    idempotency: Option<&'a str>,
    forwarded_for: Option<&'a str>,
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    opts: Call<'_>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = opts.api_key {
        builder = builder.header("x-api-key", key);
    }
    if let Some(token) = opts.bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = opts.idempotency {
        builder = builder.header("idempotency-key", key);
    }
    if let Some(ip) = opts.forwarded_for {
        builder = builder.header("x-forwarded-for", ip);
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

fn jwt(subject: &str, email: &str) -> String {
    let claims = json!({
        "iss": ISSUER,
        "sub": subject,
        "email": email,
        "exp": chrono::Utc::now().timestamp() + 3600,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(IDP_SECRET.as_bytes()),
    )
    .unwrap()
}

fn order_body(trip_id: Uuid, seat: &str, extra: Value) -> Value {
    let mut body = json!({
        "trip_id": trip_id,
        "passengers": [{"full_name": "Auth Test", "type": "adult"}],
        "items": [{"unit_code": seat, "origin": "BTG", "destination": "CEB"}],
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    body
}

#[tokio::test]
async fn auth_identity_idempotency_and_rate_limits() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    // IdP config must be in place before AppState::new reads it.
    unsafe {
        std::env::set_var("LULAN_IDP_ISSUER", ISSUER);
        std::env::set_var("LULAN_IDP_HS256_SECRET", IDP_SECRET);
        std::env::set_var("LULAN_RATE_LIMIT", "10000");
    }

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

    let trip_id: Uuid =
        sqlx::query("SELECT id FROM trips ORDER BY departs_at DESC LIMIT 1 OFFSET 2")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
    for sql in [
        "DELETE FROM scan_events WHERE ticket_id IN (SELECT id FROM tickets WHERE trip_id = $1)",
        "DELETE FROM tickets WHERE trip_id = $1",
        "DELETE FROM order_items WHERE order_id IN (SELECT id FROM orders WHERE trip_id = $1)",
        "DELETE FROM passengers WHERE order_id IN (SELECT id FROM orders WHERE trip_id = $1)",
        "DELETE FROM idempotency_keys WHERE key LIKE 'auth-it-%'",
        "DELETE FROM idempotency_keys WHERE order_id IN (SELECT id FROM orders WHERE trip_id = $1)",
        "DELETE FROM orders WHERE trip_id = $1",
        "DELETE FROM customers WHERE issuer = 'https://idp.test'",
    ] {
        sqlx::query(sql).bind(trip_id).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE seat_occupancy SET occupied_mask = 0 WHERE trip_id = $1")
        .bind(trip_id)
        .execute(&pool)
        .await
        .unwrap();

    // Redis for the rate limiter, when available.
    let redis = match std::env::var("TEST_REDIS_URL") {
        Ok(url) => match redis::Client::open(url) {
            Ok(client) => client.get_connection_manager().await.ok(),
            Err(_) => None,
        },
        Err(_) => None,
    };
    let app = lulan_api::router(AppState::new(Some(pool.clone()), redis.clone()).await);

    // ---- Role gates ----------------------------------------------------
    let (status, _) = call(&app, "GET", "/v1/webhooks", None, Call::default()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "admin surface needs a key"
    );

    let (status, minted) = call(
        &app,
        "POST",
        "/v1/api-keys",
        Some(json!({"label": "storefront", "role": "integration"})),
        Call {
            api_key: Some(ADMIN_KEY),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let integration_key = minted["key"].as_str().unwrap().to_string();

    let (status, _) = call(
        &app,
        "GET",
        "/v1/webhooks",
        None,
        Call {
            api_key: Some(&integration_key),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "integration ≠ admin");

    // ---- Guest checkout ------------------------------------------------
    let (status, body) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(order_body(trip_id, "12A", json!({}))),
        Call::default(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("guest_contact"));

    let (status, guest_order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(order_body(
            trip_id,
            "12A",
            json!({"guest_contact": "guest@example.com"}),
        )),
        Call::default(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{guest_order}");
    assert!(guest_order["customer_id"].is_null());
    let guest_id = guest_order["order_id"].as_str().unwrap().to_string();
    let retrieval = guest_order["retrieval_token"].as_str().unwrap().to_string();

    // Reads: no credential → 401; wrong token → 401; token → 200;
    // integration API key → 200.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{guest_id}"),
        None,
        Call::default(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{guest_id}?token=AAAA"),
        None,
        Call::default(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{guest_id}?token={retrieval}"),
        None,
        Call::default(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{guest_id}"),
        None,
        Call {
            api_key: Some(&integration_key),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // ---- Authenticated customer -----------------------------------------
    let token = jwt("user-42", "ana@example.com");
    let (status, customer_order) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(order_body(trip_id, "12B", json!({}))),
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{customer_order}");
    let customer_id = customer_order["customer_id"].as_str().unwrap();
    assert!(!customer_id.is_empty());
    let customer_order_id = customer_order["order_id"].as_str().unwrap();

    // Owner reads without a token; /me/orders lists it.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{customer_order_id}"),
        None,
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner reads own order");
    let (_, mine) = call(
        &app,
        "GET",
        "/v1/customers/me/orders",
        None,
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(mine.as_array().unwrap().len(), 1);

    // A different customer cannot read it.
    let stranger = jwt("user-99", "boo@example.com");
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/orders/{customer_order_id}"),
        None,
        Call {
            bearer: Some(&stranger),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Forged JWT (wrong secret) is rejected outright.
    let forged = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({"iss": ISSUER, "sub": "user-42", "exp": chrono::Utc::now().timestamp() + 3600}),
        &jsonwebtoken::EncodingKey::from_secret(b"wrong"),
    )
    .unwrap();
    let (status, _) = call(
        &app,
        "GET",
        "/v1/customers/me/orders",
        None,
        Call {
            bearer: Some(&forged),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // ---- Claiming a guest order -----------------------------------------
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/orders/{guest_id}/claim"),
        Some(json!({"retrieval_token": "AAAA"})),
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "claim needs the real token");
    let (status, claimed) = call(
        &app,
        "POST",
        &format!("/v1/orders/{guest_id}/claim"),
        Some(json!({"retrieval_token": retrieval})),
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    let (_, mine) = call(
        &app,
        "GET",
        "/v1/customers/me/orders",
        None,
        Call {
            bearer: Some(&token),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(
        mine.as_array().unwrap().len(),
        2,
        "claimed order now listed"
    );

    // ---- Idempotent booking retries ---------------------------------------
    let idem_body = order_body(
        trip_id,
        "12C",
        json!({"guest_contact": "retry@example.com"}),
    );
    let (status, first) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(idem_body.clone()),
        Call {
            idempotency: Some("auth-it-retry-1"),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, second) = call(
        &app,
        "POST",
        "/v1/orders",
        Some(idem_body),
        Call {
            idempotency: Some("auth-it-retry-1"),
            ..Call::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "replay keeps the 201");
    assert_eq!(
        first["order_id"], second["order_id"],
        "same key → same order, no double booking"
    );
    let twelve_c_orders: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_items oi
         JOIN orders o ON o.id = oi.order_id
         WHERE o.trip_id = $1 AND oi.unit_code = '12C'",
    )
    .bind(trip_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(twelve_c_orders, 1);

    // ---- Rate limiting (needs Redis) --------------------------------------
    if redis.is_some() {
        unsafe { std::env::set_var("LULAN_RATE_LIMIT", "3") };
        let mut last = StatusCode::OK;
        for _ in 0..6 {
            let (status, _) = call(
                &app,
                "POST",
                "/v1/quotes",
                Some(json!({
                    "trip_id": trip_id,
                    "items": [{"unit_code": "12D", "origin": "BTG", "destination": "CEB"}],
                })),
                Call {
                    forwarded_for: Some("203.0.113.7"),
                    ..Call::default()
                },
            )
            .await;
            last = status;
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS, "burst hits the limit");
        unsafe { std::env::set_var("LULAN_RATE_LIMIT", "10000") };
    } else {
        eprintln!("TEST_REDIS_URL not set — skipping rate-limit assertions");
    }

    // ---- Audit trail -------------------------------------------------------
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'api_key.created'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(audits >= 1);
}
