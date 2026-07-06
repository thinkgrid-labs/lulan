//! Webhook endpoint management (admin, audited). Deliveries themselves
//! are handled by `lulan_engine::webhooks` (durable queue + worker).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::auth::{AdminAuth, audit};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateWebhookRequest {
    url: String,
    /// Empty or omitted = subscribe to all event types.
    #[serde(default)]
    event_types: Vec<String>,
}

#[derive(Serialize)]
pub struct WebhookEndpoint {
    id: Uuid,
    url: String,
    event_types: Vec<String>,
    active: bool,
    /// Present only at creation — receivers verify X-Lulan-Signature
    /// with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

/// POST /v1/webhooks (admin) — register a delivery endpoint.
pub async fn create(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<WebhookEndpoint>), ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    if !req.url.starts_with("http://") && !req.url.starts_with("https://") {
        return Err(ApiError::BadRequest("url must be http(s)".into()));
    }

    let id = Uuid::new_v4();
    let secret = format!(
        "whsec_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    sqlx::query(
        "INSERT INTO webhook_endpoints (id, url, secret, event_types) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(&req.url)
    .bind(&secret)
    .bind(&req.event_types)
    .execute(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        admin.0,
        "webhook.created",
        serde_json::json!({ "id": id, "url": req.url, "event_types": req.event_types }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok((
        StatusCode::CREATED,
        Json(WebhookEndpoint {
            id,
            url: req.url,
            event_types: req.event_types,
            active: true,
            secret: Some(secret),
        }),
    ))
}

/// GET /v1/webhooks (admin) — list endpoints (secrets never repeated).
pub async fn list(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<Vec<WebhookEndpoint>>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let rows = sqlx::query(
        "SELECT id, url, event_types, active FROM webhook_endpoints ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let endpoints = rows
        .into_iter()
        .map(|row| {
            Ok(WebhookEndpoint {
                id: row.try_get("id")?,
                url: row.try_get("url")?,
                event_types: row.try_get("event_types")?,
                active: row.try_get("active")?,
                secret: None,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(endpoints))
}

/// DELETE /v1/webhooks/{id} (admin) — deactivate an endpoint.
pub async fn remove(
    State(state): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let updated = sqlx::query("UPDATE webhook_endpoints SET active = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("webhook {id} not found")));
    }
    audit(
        pool,
        admin.0,
        "webhook.deactivated",
        serde_json::json!({ "id": id }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(serde_json::json!({ "deactivated": id })))
}
