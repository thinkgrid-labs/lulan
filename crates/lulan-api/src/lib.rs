//! Lulan API: HTTP layer over the engine. Library crate so integration
//! tests (and later the loadgen harness) can drive the router directly.

pub mod auth;
pub mod config;
pub mod error;
pub mod health;
pub mod identity;
pub mod metrics;
pub mod orders;
pub mod pricing;
pub mod quotes;
pub mod rate_limit;
pub mod reservations;
pub mod seed;
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

/// The committed OpenAPI spec (docs/openapi.json), served at
/// /openapi.json. Hand-authored until the API stabilises pre-v1; the
/// `openapi_documents_only_real_routes` test keeps it from drifting off
/// the actual router.
pub const OPENAPI_JSON: &str = include_str!("../../../docs/openapi.json");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/v1/trips/search", get(trips::search))
        .route("/v1/trips/{trip_id}/availability", get(trips::availability))
        .route("/v1/trips/{trip_id}/holds", post(reservations::create_hold))
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
        .route("/v1/api-keys", post(auth::create_key))
        .route("/v1/api-keys/{id}", axum::routing::delete(auth::revoke_key))
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
        .layer(axum::middleware::from_fn(metrics::record))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
                    .uri("/v1/trips/search?origin=BTG&destination=CEB&date=2026-07-06")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
