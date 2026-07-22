//! Lulan API: HTTP layer over the engine. Library crate so integration
//! tests (and later the loadgen harness) can drive the router directly.

pub mod admin;
pub mod ancillaries;
pub mod auth;
pub mod config;
pub mod cors;
pub mod error;
pub mod gtfs;
pub mod health;
pub mod idempotency;
pub mod identity;
pub mod metrics;
pub mod orders;
pub mod pricing;
pub mod quotes;
pub mod rate_limit;
pub mod reservations;
pub mod seed;
pub mod staff;
pub mod state;
pub mod tickets;
pub mod trips;
pub mod webhooks_admin;

use axum::{
    Router,
    routing::{delete, get, post},
};
use tower_http::trace::TraceLayer;

use state::AppState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The committed OpenAPI spec (crates/lulan-api/openapi.json), served at
/// /openapi.json. Hand-authored until the API stabilises pre-v1; the
/// `openapi_documents_only_real_routes` test keeps it from drifting off
/// the actual router.
pub const OPENAPI_JSON: &str = include_str!("../openapi.json");

pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/v1/trips/search", get(trips::search))
        .route("/v1/trips/{trip_id}/availability", get(trips::availability))
        .route("/v1/holds", post(reservations::create_hold))
        .route(
            "/v1/trips/{trip_id}/claims",
            post(reservations::create_claim),
        )
        .route("/v1/holds/{hold_id}", delete(reservations::release_hold))
        .route("/v1/quotes", post(quotes::create))
        .route("/v1/orders", post(orders::create))
        .route("/v1/orders/{order_id}", get(orders::get))
        .route(
            "/v1/orders/{order_id}/payment",
            post(orders::request_payment),
        )
        .route("/v1/orders/{order_id}/cancel", post(orders::cancel))
        .route("/v1/payments/fake/webhook", post(orders::fake_webhook))
        .route(
            "/v1/orders/{order_id}/tickets",
            post(tickets::issue).get(tickets::list),
        )
        .route("/v1/ticket-keys", get(tickets::keys))
        .route("/v1/scans", post(tickets::sync))
        .route("/v1/orders/{order_id}/claim", post(identity::claim_order))
        .route("/v1/customers/me/orders", get(identity::my_orders))
        .route(
            "/v1/webhooks",
            post(webhooks_admin::create).get(webhooks_admin::list),
        )
        .route(
            "/v1/webhooks/{id}",
            axum::routing::delete(webhooks_admin::remove),
        )
        .route(
            "/v1/ancillaries",
            get(ancillaries::list).post(ancillaries::create),
        )
        .route(
            "/v1/ancillaries/{id}",
            axum::routing::delete(ancillaries::remove),
        )
        .route("/v1/api-keys", post(auth::create_key))
        .route("/v1/api-keys/{id}", axum::routing::delete(auth::revoke_key))
        .route(
            "/v1/admin/staff",
            post(admin::enroll_staff).get(admin::list_staff),
        )
        .route(
            "/v1/admin/staff/{id}",
            axum::routing::delete(admin::revoke_staff),
        )
        .route(
            "/v1/admin/fare-rules",
            post(admin::publish_fare_rules).get(admin::list_fare_rules),
        )
        .route(
            "/v1/admin/fare-rules/{id}/activate",
            post(admin::activate_fare_rules),
        )
        .route("/v1/admin/locations", post(admin::create_location))
        .route("/v1/admin/routes", post(admin::create_route))
        .route("/v1/admin/vessels", post(admin::create_vessel))
        .route("/v1/admin/trips", post(admin::create_trips))
        .route("/v1/admin/trips/{id}/cancel", post(admin::cancel_trip))
        .route("/v1/admin/trips/{id}/manifest", get(admin::trip_manifest))
        .route("/v1/admin/orders", get(admin::search_orders))
        .route("/v1/admin/orders/{id}/refund", post(admin::refund_order))
        .route("/metrics", get(metrics::render))
        .route(
            "/openapi.json",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    OPENAPI_JSON,
                )
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::limit,
        ))
        .layer(axum::middleware::from_fn(metrics::record));

    // Browser access is opt-in (see `cors`). Applied outside the rate
    // limiter so a preflight never spends a caller's budget, and only when
    // configured — an empty CorsLayer would still answer every OPTIONS.
    let router = match cors::layer_from_env() {
        Some(cors) => router.layer(cors),
        None => router,
    };

    router.layer(TraceLayer::new_for_http()).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_endpoints_respond_without_a_database() {
        let app = router(AppState::new(None, None).await);

        for path in ["/health/live", "/health/ready"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    /// Drift check: every documented (path, method) must resolve to a real
    /// handler — a route miss (404 with axum's empty fallback) or a method
    /// mismatch (405) fails. Handlers legitimately return 400/401/415/503
    /// on an infra-less boot; that's fine, the route exists.
    #[tokio::test]
    async fn openapi_documents_only_real_routes() {
        let app = router(AppState::new(None, None).await);
        let spec: serde_json::Value = serde_json::from_str(OPENAPI_JSON).expect("spec parses");
        let uuid = "00000000-0000-0000-0000-000000000000";
        for (path, ops) in spec["paths"].as_object().unwrap() {
            let mut concrete = String::new();
            let mut in_param = false;
            for ch in path.chars() {
                match ch {
                    '{' => in_param = true,
                    '}' => {
                        in_param = false;
                        concrete.push_str(uuid);
                    }
                    c if !in_param => concrete.push(c),
                    _ => {}
                }
            }
            for method in ops.as_object().unwrap().keys() {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.to_uppercase().as_str())
                            .uri(&concrete)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_ne!(
                    response.status(),
                    StatusCode::NOT_FOUND,
                    "{method} {path} documented but not routed"
                );
                assert_ne!(
                    response.status(),
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} documented with wrong method"
                );
            }
        }
    }

    #[tokio::test]
    async fn trip_endpoints_report_unavailable_without_a_database() {
        let app = router(AppState::new(None, None).await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/trips/search?origin=BTG&destination=CEB&departure_date=2026-07-06")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
