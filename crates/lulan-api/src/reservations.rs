//! Hold and claim endpoints — the Phase 2 reservation surface.
//!
//! Flow: `POST holds` (fast, Redis, expires) → `POST claims` (Postgres,
//! authoritative). Claims never require a hold; a hold only protects the
//! span from other *holds* while the customer fills in details.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lulan_engine::inventory::ClaimOutcome;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_HOLD_TTL_SECS: u64 = 600;
const MAX_HOLD_TTL_SECS: u64 = 1800;

#[derive(Deserialize)]
pub struct HoldRequest {
    unit_code: String,
    origin: String,
    destination: String,
    ttl_seconds: Option<u64>,
}

#[derive(Serialize)]
pub struct HoldResponse {
    hold_id: Uuid,
    trip_id: Uuid,
    unit_code: String,
    expires_at: DateTime<Utc>,
}

/// POST /v1/trips/{trip_id}/holds
pub async fn create_hold(
    State(state): State<AppState>,
    Path(trip_id): Path<Uuid>,
    Json(req): Json<HoldRequest>,
) -> Result<(StatusCode, Json<HoldResponse>), ApiError> {
    let store = state.inventory()?;
    let holds = state.holds()?;

    let target = store
        .resolve_target(trip_id, &req.unit_code, &req.origin, &req.destination)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "trip {trip_id} with unit {:?} not found",
                req.unit_code
            ))
        })?;
    if target.kind != "seat" {
        return Err(ApiError::BadRequest(
            "holds are only supported for seats; claim pools directly".into(),
        ));
    }

    // Already-sold segments can never be held — check the source of truth
    // first so customers aren't strung along on a dead span.
    let occupied = store
        .seat_mask(trip_id, target.unit_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("no occupancy row for this trip/unit".into()))?;
    if !target.span.is_available(occupied) {
        return Err(ApiError::Conflict("span already sold for this seat".into()));
    }

    let ttl = std::time::Duration::from_secs(
        req.ttl_seconds
            .unwrap_or(DEFAULT_HOLD_TTL_SECS)
            .min(MAX_HOLD_TTL_SECS),
    );
    let hold = holds
        .acquire(trip_id, target.unit_id, target.span, ttl)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .ok_or_else(|| ApiError::Conflict("span currently held by another session".into()))?;

    Ok((
        StatusCode::CREATED,
        Json(HoldResponse {
            hold_id: hold.hold_id,
            trip_id,
            unit_code: req.unit_code,
            expires_at: hold.expires_at,
        }),
    ))
}

/// DELETE /v1/holds/{hold_id}
pub async fn release_hold(
    State(state): State<AppState>,
    Path(hold_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let holds = state.holds()?;
    let released = holds
        .release(hold_id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    if released {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("hold not found or expired".into()))
    }
}

#[derive(Deserialize)]
pub struct ClaimRequest {
    unit_code: String,
    origin: String,
    destination: String,
    /// Pool claims only; defaults to 1. Ignored for seats.
    quantity: Option<i32>,
    /// Optional hold to consume. Verified when Redis is available; a claim
    /// is authoritative with or without it.
    hold_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct ClaimResponse {
    status: &'static str,
    trip_id: Uuid,
    unit_code: String,
    from_index: u8,
    to_index: u8,
}

/// POST /v1/trips/{trip_id}/claims
pub async fn create_claim(
    State(state): State<AppState>,
    Path(trip_id): Path<Uuid>,
    Json(req): Json<ClaimRequest>,
) -> Result<(StatusCode, Json<ClaimResponse>), ApiError> {
    let store = state.inventory()?;

    let target = store
        .resolve_target(trip_id, &req.unit_code, &req.origin, &req.destination)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "trip {trip_id} with unit {:?} not found",
                req.unit_code
            ))
        })?;

    // A presented hold must be live and must match this trip + unit — but
    // Redis being down never blocks a claim (ADR 0002): if verification
    // itself fails, we proceed and let the guarded UPDATE decide.
    if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
        match holds.verify(hold_id, trip_id, target.unit_id).await {
            Ok(false) => {
                return Err(ApiError::Conflict(
                    "hold expired or does not match this trip/unit".into(),
                ));
            }
            Ok(true) => {}
            Err(err) => {
                tracing::warn!(error = %err, %hold_id, "hold verification unavailable — proceeding to claim");
            }
        }
    }

    let outcome = match target.kind.as_str() {
        "seat" => {
            store
                .claim_seat(trip_id, target.unit_id, target.span)
                .await?
        }
        "pool" => {
            let qty = req.quantity.unwrap_or(1);
            if qty <= 0 {
                return Err(ApiError::BadRequest("quantity must be positive".into()));
            }
            store
                .claim_pool(trip_id, target.unit_id, target.span, qty)
                .await?
        }
        other => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "unknown capacity unit kind {other:?}"
            )));
        }
    };

    match outcome {
        ClaimOutcome::Claimed => {
            // The span is now sold; the hold has served its purpose.
            if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
                let _ = holds.release(hold_id).await;
            }
            Ok((
                StatusCode::CREATED,
                Json(ClaimResponse {
                    status: "claimed",
                    trip_id,
                    unit_code: req.unit_code,
                    from_index: target.span.from_index(),
                    to_index: target.span.to_index(),
                }),
            ))
        }
        ClaimOutcome::Conflict => Err(ApiError::Conflict(
            "no longer available for the requested span".into(),
        )),
        ClaimOutcome::NotFound => Err(ApiError::NotFound(
            "no occupancy row for this trip/unit".into(),
        )),
    }
}
