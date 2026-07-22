//! Ticketing endpoints (Phase 5): ticket retrieval/issue, the public key
//! set validators cache for offline verification, and the boarding scan
//! sync devices upload when connectivity returns.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use lulan_engine::ticket::{IssuedTicket, PublicKeyEntry, ScanOutcome, TicketError, TicketStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// GET /v1/ticket-keys — every signing key's public half. Crew
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
/// the retry path and the fetch-with-issue for clients. Tickets are
/// boarding passes: reads are gated like the order itself.
pub async fn issue(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
    Query(params): Query<crate::orders::GetOrderParams>,
    headers: HeaderMap,
) -> Result<Json<TicketsResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    crate::orders::authorize_order_access(&state, &headers, order_id, params.token()).await?;
    match TicketStore::new(pool.clone())
        .issue_for_order(order_id)
        .await
    {
        Ok(tickets) => Ok(Json(TicketsResponse { order_id, tickets })),
        Err(TicketError::OrderNotFound) => {
            Err(ApiError::NotFound(format!("order {order_id} not found")))
        }
        Err(TicketError::NotPaid(status)) => Err(ApiError::Conflict(format!(
            "tickets require a paid order; current state is {status:?}"
        ))),
        Err(TicketError::NoSigningKey) => {
            Err(ApiError::ServiceUnavailable("no active ticket signing key"))
        }
        Err(err) => Err(ApiError::Internal(err.into())),
    }
}

/// GET /v1/orders/{order_id}/tickets — fetch without issuing.
pub async fn list(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
    Query(params): Query<crate::orders::GetOrderParams>,
    headers: HeaderMap,
) -> Result<Json<TicketsResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    crate::orders::authorize_order_access(&state, &headers, order_id, params.token()).await?;
    let tickets = TicketStore::new(pool.clone())
        .tickets_for_order(order_id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(TicketsResponse { order_id, tickets }))
}

/// How far ahead a device's revocation list reaches. Long enough to cover
/// a shift and an overnight cache; short enough that the list stays small.
const REVOCATION_HORIZON_HOURS: i64 = 72;

#[derive(Deserialize)]
pub struct RevocationParams {
    /// Narrow the list to one departure — what a gate device should do.
    /// Scoped answers are complete regardless of when the trip departs;
    /// the unscoped list is bounded to the next few days.
    #[serde(default)]
    trip_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct RevocationsResponse {
    /// Ticket ids to refuse despite a valid signature.
    revoked: Vec<Uuid>,
    /// When this list was produced; a device shows staleness from it.
    as_of: DateTime<Utc>,
    horizon_hours: i64,
}

/// GET /v1/revocations — tickets that must be refused even though they
/// verify (refunded orders, cancelled trips, voided seats).
///
/// A signature proves a ticket was issued, never that it is still good:
/// the cancellation happens after signing, so no offline check can derive
/// it. Devices cache this next to `GET /v1/ticket-keys` and pass it to
/// `lulan_validate::verify_ticket_with_revocations`. Coverage is bounded
/// by how recently the device synced — that limit is the honest one, and
/// it is the same one clone detection lives with.
pub async fn revocations(
    State(state): State<AppState>,
    _device: crate::auth::DeviceAuth,
    Query(params): Query<RevocationParams>,
) -> Result<Json<RevocationsResponse>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let revoked = TicketStore::new(pool.clone())
        .revoked_tickets(params.trip_id, REVOCATION_HORIZON_HOURS)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(RevocationsResponse {
        revoked,
        as_of: Utc::now(),
        horizon_hours: REVOCATION_HORIZON_HOURS,
    }))
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

/// POST /v1/scans — batched, idempotent boarding sync from validator
/// devices (validator or admin API key). Safe to replay entire journals.
pub async fn sync(
    State(state): State<AppState>,
    _device: crate::auth::DeviceAuth,
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
