use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::NaiveDate;
use lulan_engine::inventory::{TripAvailability, TripSummary};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TripType {
    OneWay,
    RoundTrip,
}

fn default_trip_type() -> TripType {
    TripType::OneWay
}

#[derive(Deserialize)]
pub struct SearchParams {
    origin: String,
    destination: String,
    /// Outbound service date, `YYYY-MM-DD`.
    departure_date: NaiveDate,
    /// Required for `round_trip`, forbidden for `one_way`.
    #[serde(default)]
    return_date: Option<NaiveDate>,
    #[serde(default = "default_trip_type")]
    trip_type: TripType,
}

/// One leg of the search: a direction + date and its candidate trips.
#[derive(Serialize)]
pub struct SearchLeg {
    /// `outbound` or `return`.
    leg: &'static str,
    origin: String,
    destination: String,
    date: NaiveDate,
    trips: Vec<TripSummary>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    trip_type: TripType,
    legs: Vec<SearchLeg>,
}

/// GET /v1/trips/search — availability for a one-way or round-trip
/// itinerary. Each leg returns its own candidate trips (pick one per leg
/// to build a `journeys` quote/order). Multi-city is composed client-side
/// from several one-way searches.
///
/// `?origin=BTG&destination=CEB&departure_date=2026-07-11`
/// `&trip_type=round_trip&return_date=2026-07-13`
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    // Trip type drives validation: the return date is required for a round
    // trip and meaningless for a one-way.
    match params.trip_type {
        TripType::OneWay if params.return_date.is_some() => {
            return Err(ApiError::BadRequest(
                "return_date is only valid for trip_type=round_trip".into(),
            ));
        }
        TripType::RoundTrip if params.return_date.is_none() => {
            return Err(ApiError::BadRequest(
                "round_trip requires a return_date".into(),
            ));
        }
        _ => {}
    }

    let store = state.inventory()?;
    let mut legs = vec![SearchLeg {
        leg: "outbound",
        origin: params.origin.clone(),
        destination: params.destination.clone(),
        date: params.departure_date,
        trips: store
            .search_trips(&params.origin, &params.destination, params.departure_date)
            .await?,
    }];

    if let Some(return_date) = params.return_date {
        legs.push(SearchLeg {
            leg: "return",
            origin: params.destination.clone(),
            destination: params.origin.clone(),
            date: return_date,
            trips: store
                .search_trips(&params.destination, &params.origin, return_date)
                .await?,
        });
    }

    Ok(Json(SearchResponse {
        trip_type: params.trip_type,
        legs,
    }))
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
