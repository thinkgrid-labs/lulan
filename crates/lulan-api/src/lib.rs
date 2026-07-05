//! Lulan API: HTTP layer over the engine. Library crate so integration
//! tests (and later the loadgen harness) can drive the router directly.

pub mod config;
pub mod error;
pub mod health;
pub mod reservations;
pub mod seed;
pub mod state;
pub mod trips;

use axum::{
    Router,
    routing::{delete, get, post},
};
use tower_http::trace::TraceLayer;

use state::AppState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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
        let app = router(AppState {
            db: None,
            redis: None,
        });

        for path in ["/health/live", "/health/ready"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn trip_endpoints_report_unavailable_without_a_database() {
        let app = router(AppState {
            db: None,
            redis: None,
        });
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
