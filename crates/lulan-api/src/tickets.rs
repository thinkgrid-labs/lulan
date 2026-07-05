//! Ticketing endpoints (Phase 5): ticket retrieval/issue, the public key
//! set validators cache for offline verification, and the boarding scan
//! sync devices upload when connectivity returns.

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use lulan_engine::ticket::{IssuedTicket, PublicKeyEntry, ScanOutcome, TicketError, TicketStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// GET /v1/ticket-keys — every signing key's public half. Conductor
/// devices cache this while online; validation then needs no server.
pub async fn keys(State(state): State<AppState>) -> Result<Json<KeysResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let keys = lulan_engine::ticket::public_keys(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(KeysResponse { keys }))
}

#[derive(Serialize)]
pub struct KeysResponse {
    keys: Vec<PublicKeyEntry>,
}

#[derive(Serialize)]
pub struct TicketsResponse {
    order_id: Uuid,
    tickets: Vec<IssuedTicket>,
}

/// POST /v1/orders/{order_id}/tickets — issue (idempotent) and return.
/// Normally tickets are issued automatically on payment capture; this is
/// the retry path and the fetch-with-issue for clients.
pub async fn issue(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<TicketsResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let signer = state
        .ticket_signer
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("ticket signing unavailable"))?;
    match TicketStore::new(pool.clone())
        .issue_for_order(order_id, signer)
        .await
    {
        Ok(tickets) => Ok(Json(TicketsResponse { order_id, tickets })),
        Err(TicketError::OrderNotFound) => {
            Err(ApiError::NotFound(format!("order {order_id} not found")))
        }
        Err(TicketError::NotPaid(status)) => Err(ApiError::Conflict(format!(
            "tickets require a paid order; current state is {status:?}"
        ))),
        Err(err) => Err(ApiError::Internal(err.into())),
    }
}

/// GET /v1/orders/{order_id}/tickets — fetch without issuing.
pub async fn list(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<TicketsResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let tickets = TicketStore::new(pool.clone())
        .tickets_for_order(order_id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(TicketsResponse { order_id, tickets }))
}

#[derive(Deserialize)]
pub struct ScanSyncRequest {
    device_id: String,
    scans: Vec<ScanRequest>,
}

#[derive(Deserialize)]
pub struct ScanRequest {
    ticket_id: Uuid,
    scanned_at: DateTime<Utc>,
    /// The device's local verdict (`ok`, `expired`, …) — journaled as-is.
    #[serde(default = "default_result")]
    result: String,
}

fn default_result() -> String {
    "ok".into()
}

#[derive(Serialize)]
pub struct ScanSyncResponse {
    outcomes: Vec<ScanOutcome>,
}

/// POST /v1/scans — batched, idempotent boarding sync from conductor
/// devices. Safe to replay entire journals.
pub async fn sync(
    State(state): State<AppState>,
    Json(req): Json<ScanSyncRequest>,
) -> Result<Json<ScanSyncResponse>, ApiError> {
    if req.device_id.trim().is_empty() {
        return Err(ApiError::BadRequest("device_id is required".into()));
    }
    if req.scans.len() > 1000 {
        return Err(ApiError::BadRequest("max 1000 scans per batch".into()));
    }
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let store = TicketStore::new(pool.clone());

    let mut outcomes = Vec::with_capacity(req.scans.len());
    for scan in &req.scans {
        let outcome = store
            .record_scan(
                scan.ticket_id,
                &req.device_id,
                scan.scanned_at,
                &scan.result,
            )
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        outcomes.push(outcome);
    }
    Ok(Json(ScanSyncResponse { outcomes }))
}
