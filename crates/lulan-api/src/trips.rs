use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::NaiveDate;
use lulan_engine::inventory::{TripAvailability, TripSummary};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct SearchParams {
    origin: String,
    destination: String,
    /// Service date, `YYYY-MM-DD`.
    date: NaiveDate,
}

#[derive(Serialize)]
pub struct SearchResponse {
    trips: Vec<TripSummary>,
}

/// GET /v1/trips/search?origin=BTG&destination=CEB&date=2026-07-06
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let store = state.inventory()?;
    let trips = store
        .search_trips(&params.origin, &params.destination, params.date)
        .await?;
    Ok(Json(SearchResponse { trips }))
}

#[derive(Deserialize)]
pub struct AvailabilityParams {
    origin: String,
    destination: String,
}

/// GET /v1/trips/{trip_id}/availability?origin=BTG&destination=CTC
pub async fn availability(
    State(state): State<AppState>,
    Path(trip_id): Path<Uuid>,
    Query(params): Query<AvailabilityParams>,
) -> Result<Json<TripAvailability>, ApiError> {
    let store = state.inventory()?;
    let availability = store
        .trip_availability(trip_id, &params.origin, &params.destination)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("trip {trip_id} not found")))?;
    Ok(Json(availability))
}
