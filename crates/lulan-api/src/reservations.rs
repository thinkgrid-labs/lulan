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

/// Operator-configurable hold window (admin-settable via env; a runtime
/// settings API arrives with the Phase 7.5 admin surface). Holds ALWAYS
/// expire — an eternal hold is an inventory denial-of-service — so the
/// requested TTL is clamped to [MIN, max].
const DEFAULT_HOLD_TTL_SECS: u64 = 600;
const MAX_HOLD_TTL_SECS: u64 = 1800;
const MIN_HOLD_TTL_SECS: u64 = 30;

fn default_hold_ttl() -> u64 {
    std::env::var("LULAN_HOLD_DEFAULT_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HOLD_TTL_SECS)
}

fn max_hold_ttl() -> u64 {
    std::env::var("LULAN_HOLD_MAX_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_HOLD_TTL_SECS)
}

/// Stampede control (plan §Phase 2). `POST /v1/holds` needs no credential,
/// so nothing else stops one session from soft-holding a whole fleet for
/// half an hour and taking it off sale.
///
/// Two blunt limits, neither of which needs to know who the caller is —
/// which matters, because the alternatives (API key, IP) are absent or
/// spoofable here:
///
/// 1. A cap on seats per request.
/// 2. A ceiling on how much of one trip may be held at once.
///
/// The second degrades honestly under genuine peak load: at the ceiling,
/// new holds are refused. That is survivable precisely because **a hold is
/// not a sale** — claims are authoritative and unaffected, so a customer
/// refused a hold can still complete a booking, just without the seat
/// being reserved while they type.
const MAX_SEATS_PER_HOLD: usize = 20;
const DEFAULT_TRIP_HOLD_FRACTION: f64 = 0.75;

/// Fraction of a trip's seats that may carry a hold simultaneously.
/// `LULAN_HOLD_MAX_TRIP_FRACTION`; 0 or >= 1 disables the ceiling.
fn trip_hold_fraction() -> f64 {
    std::env::var("LULAN_HOLD_MAX_TRIP_FRACTION")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|f: &f64| f.is_finite() && *f > 0.0)
        .unwrap_or(DEFAULT_TRIP_HOLD_FRACTION)
}

/// One seat to hold. Holds cover seats only; pools are claimed at order
/// time.
#[derive(Deserialize)]
pub struct HoldItem {
    unit_code: String,
    origin: String,
    destination: String,
}

#[derive(Deserialize)]
pub struct HoldJourney {
    trip_id: Uuid,
    items: Vec<HoldItem>,
}

/// Itinerary shape (`journeys`) or the single-trip shape (`trip_id` +
/// `items`) — the same shapes as quotes and orders. A one-way holds one
/// journey; a round trip holds two. One call, one hold id.
#[derive(Deserialize)]
pub struct HoldRequest {
    #[serde(default)]
    trip_id: Option<Uuid>,
    #[serde(default)]
    items: Option<Vec<HoldItem>>,
    #[serde(default)]
    journeys: Option<Vec<HoldJourney>>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

#[derive(Serialize)]
pub struct HeldItemInfo {
    trip_id: Uuid,
    unit_code: String,
    origin: String,
    destination: String,
}

#[derive(Serialize)]
pub struct HoldResponse {
    /// One id for the whole itinerary hold — present it at order time (or
    /// to DELETE /v1/holds/{id}) to release every seat together.
    hold_id: Uuid,
    expires_at: DateTime<Utc>,
    items: Vec<HeldItemInfo>,
}

/// POST /v1/holds — soft-hold every seat of a one-way or round-trip
/// selection as one itinerary hold. All-or-nothing: if any seat is already
/// held or sold, nothing is held (409).
pub async fn create_hold(
    State(state): State<AppState>,
    Json(req): Json<HoldRequest>,
) -> Result<(StatusCode, Json<HoldResponse>), ApiError> {
    let flat: Vec<(Uuid, HoldItem)> = match (req.trip_id, req.items, req.journeys) {
        (None, None, Some(journeys)) => {
            if journeys.is_empty() || journeys.iter().any(|j| j.items.is_empty()) {
                return Err(ApiError::BadRequest(
                    "every journey needs at least one item".into(),
                ));
            }
            if journeys.len() > 8 {
                return Err(ApiError::BadRequest("max 8 journeys per itinerary".into()));
            }
            journeys
                .into_iter()
                .flat_map(|j| {
                    let trip_id = j.trip_id;
                    j.items.into_iter().map(move |i| (trip_id, i))
                })
                .collect()
        }
        (Some(trip_id), Some(items), None) if !items.is_empty() => {
            items.into_iter().map(|i| (trip_id, i)).collect()
        }
        _ => {
            return Err(ApiError::BadRequest(
                "provide either journeys[] or trip_id + items".into(),
            ));
        }
    };

    if flat.len() > MAX_SEATS_PER_HOLD {
        return Err(ApiError::BadRequest(format!(
            "a hold covers at most {MAX_SEATS_PER_HOLD} seats; split larger \
             selections across separate holds"
        )));
    }

    let store = state.inventory()?;
    let holds = state.holds()?;

    // Resolve every seat and reject dead spans against the source of truth
    // before touching Redis.
    let mut seats = Vec::with_capacity(flat.len());
    let mut infos = Vec::with_capacity(flat.len());
    for (trip_id, item) in &flat {
        let target = store
            .resolve_target(*trip_id, &item.unit_code, &item.origin, &item.destination)
            .await?
            .ok_or_else(|| {
                ApiError::NotFound(format!(
                    "trip {trip_id} with unit {:?} not found",
                    item.unit_code
                ))
            })?;
        if target.kind != "seat" {
            return Err(ApiError::BadRequest(
                "holds are only supported for seats; pools are claimed at order time".into(),
            ));
        }
        let occupied = store
            .seat_mask(*trip_id, target.unit_id)
            .await?
            .ok_or_else(|| ApiError::NotFound("no occupancy row for this trip/unit".into()))?;
        if !target.span.is_available(occupied) {
            return Err(ApiError::Conflict(format!(
                "span already sold for seat {} on trip {trip_id}",
                item.unit_code
            )));
        }
        seats.push((*trip_id, target.unit_id, target.span));
        infos.push(HeldItemInfo {
            trip_id: *trip_id,
            unit_code: item.unit_code.clone(),
            origin: item.origin.clone(),
            destination: item.destination.clone(),
        });
    }

    // Per-trip ceiling. Checked per distinct trip so a round trip is
    // judged on each leg's own pressure, and only when a ceiling is in
    // force. A failure to measure never blocks the hold: this is abuse
    // control, and like the rate limiter it must not become the outage.
    let fraction = trip_hold_fraction();
    if fraction < 1.0 {
        let mut wanted: std::collections::HashMap<Uuid, usize> = std::collections::HashMap::new();
        for (trip_id, _, _) in &seats {
            *wanted.entry(*trip_id).or_default() += 1;
        }
        for (trip_id, adding) in wanted {
            let total = store.seat_count(trip_id).await?;
            if total == 0 {
                continue;
            }
            let ceiling = ((total as f64) * fraction).floor().max(1.0) as usize;
            match holds.held_unit_count(trip_id, ceiling).await {
                Ok(held) if held + adding > ceiling => {
                    tracing::info!(
                        %trip_id, held, adding, ceiling,
                        "hold refused: trip is at its hold ceiling"
                    );
                    return Err(ApiError::Conflict(format!(
                        "too many seats on trip {trip_id} are being held right now — \
                         try again shortly, or book without a hold (holds never gate \
                         the sale)"
                    )));
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, %trip_id, "hold pressure unknown — allowing");
                }
            }
        }
    }

    let ttl = std::time::Duration::from_secs(
        req.ttl_seconds
            .unwrap_or_else(default_hold_ttl)
            .clamp(MIN_HOLD_TTL_SECS, max_hold_ttl()),
    );
    let hold = holds
        .acquire_itinerary(&seats, ttl)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .ok_or_else(|| {
            ApiError::Conflict(
                "a seat in this itinerary is currently held by another session".into(),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(HoldResponse {
            hold_id: hold.hold_id,
            expires_at: hold.expires_at,
            items: infos,
        }),
    ))
}

/// DELETE /v1/holds/{hold_id} — release the whole itinerary hold.
pub async fn release_hold(
    State(state): State<AppState>,
    Path(hold_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let holds = state.holds()?;
    let released = holds
        .release_itinerary(hold_id)
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
///
/// Requires an `integration` or `operator_admin` key. A claim here sells
/// capacity outside the order lifecycle: it has no expiry, no payment, and
/// no release endpoint, so it must never be reachable anonymously. Public
/// booking goes through `POST /v1/orders`, whose claims are provisional
/// and swept on expiry.
pub async fn create_claim(
    State(state): State<AppState>,
    _auth: crate::auth::IntegrationAuth,
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

    // A presented itinerary hold must be live and must cover this trip +
    // unit — but Redis being down never blocks a claim (ADR 0002): if
    // verification itself fails, we proceed and let the guarded UPDATE
    // decide.
    if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
        match holds.itinerary_members(hold_id).await {
            Ok(Some(members)) => {
                let covers = members
                    .iter()
                    .any(|m| m.trip_id == trip_id && m.unit_id == target.unit_id);
                if !covers {
                    return Err(ApiError::Conflict(
                        "hold does not cover this trip/unit".into(),
                    ));
                }
            }
            Ok(None) => {
                return Err(ApiError::Conflict("hold expired or unknown".into()));
            }
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
            // The span is now sold; the itinerary hold has served its
            // purpose. (This low-level single-seat claim releases the whole
            // hold — fine for its single-seat use; orders are the
            // multi-seat path.)
            if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
                let _ = holds.release_itinerary(hold_id).await;
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
