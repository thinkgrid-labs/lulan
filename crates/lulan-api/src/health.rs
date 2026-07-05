use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct Health {
    status: &'static str,
    checks: Checks,
}

#[derive(Serialize)]
pub struct Checks {
    database: &'static str,
    redis: &'static str,
}

/// Liveness: the process is up and serving requests.
pub async fn live() -> Json<Health> {
    Json(Health {
        status: "ok",
        checks: Checks {
            database: "skipped",
            redis: "skipped",
        },
    })
}

/// Readiness: configured dependencies are reachable. Unconfigured ones are
/// reported as skipped, not failed (infra-less boot stays ready).
pub async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let database = match &state.db {
        None => "skipped",
        Some(pool) => match sqlx::query("SELECT 1").execute(pool).await {
            Ok(_) => "ok",
            Err(err) => {
                tracing::error!(error = %err, "readiness: database check failed");
                "error"
            }
        },
    };

    let redis = match &state.redis {
        None => "skipped",
        Some(conn) => {
            let mut conn = conn.clone();
            match redis::cmd("PING").query_async::<String>(&mut conn).await {
                Ok(_) => "ok",
                Err(err) => {
                    tracing::error!(error = %err, "readiness: redis check failed");
                    "error"
                }
            }
        }
    };

    let healthy = database != "error" && redis != "error";
    (
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(Health {
            status: if healthy { "ok" } else { "unavailable" },
            checks: Checks { database, redis },
        }),
    )
}
