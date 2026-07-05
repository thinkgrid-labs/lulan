//! Pricing glue: loads the active fare rules, derives the pure-engine
//! scalars (weekday, advance days, occupancy, itinerary context) from
//! live data, and prices line items through whichever [`PricingEngine`]
//! the server booted with.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use lulan_engine::inventory::InventoryStore;
use lulan_pricing::rules::{FareRuleSet, Quote, RuleInput};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// One item to price, as clients describe it inside a journey.
#[derive(Debug, Clone, Deserialize)]
pub struct JourneyItem {
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    #[serde(default)]
    pub quantity: Option<i32>,
    /// Travelling passenger's type for seat items (drives concession
    /// fares); ignored for pools.
    #[serde(default)]
    pub passenger_type: Option<String>,
}

/// A journey-resolved item ready for pricing.
#[derive(Debug, Clone)]
pub struct PriceableItem {
    pub trip_id: Uuid,
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub quantity: Option<i32>,
    pub passenger_type: Option<String>,
}

/// A priced line item, span-resolved.
pub struct PricedItem {
    pub trip_id: Uuid,
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub from_index: u8,
    pub to_index: u8,
    pub quantity: i32,
    pub passenger_type: Option<String>,
    pub quote: Quote,
}

/// Itinerary context, host-derived so pricing modules stay deterministic.
#[derive(Debug, Clone, Copy)]
pub struct JourneyContext {
    pub journey_count: u32,
    pub is_round_trip: bool,
}

/// Trip type is derived, never declared: one journey = one-way; exactly
/// two journeys where the second reverses the first's O&D (judged by each
/// journey's first item) = round-trip; anything else = multi-city.
pub fn journey_context(journeys: &[(Uuid, Vec<JourneyItem>)]) -> JourneyContext {
    let is_round_trip = journeys.len() == 2
        && journeys[0].0 != journeys[1].0
        && match (journeys[0].1.first(), journeys[1].1.first()) {
            (Some(out), Some(back)) => {
                out.origin == back.destination && out.destination == back.origin
            }
            _ => false,
        };
    JourneyContext {
        journey_count: journeys.len() as u32,
        is_round_trip,
    }
}

/// The newest active ruleset. No rules configured is an operator error —
/// selling at an undefined price is worse than refusing.
pub async fn load_rules(pool: &PgPool) -> Result<FareRuleSet, ApiError> {
    let row: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT rules FROM fare_rules WHERE active ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let value = row.ok_or(ApiError::ServiceUnavailable(
        "no active fare rules configured",
    ))?;
    serde_json::from_value(value)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("stored fare rules are malformed: {e}")))
}

/// Price every item of a prospective itinerary against live occupancy.
/// Each item prices on its own trip's calendar (weekday, advance days).
pub async fn price_items(
    state: &AppState,
    items: &[PriceableItem],
    promo_code: Option<&str>,
    context: JourneyContext,
) -> Result<Vec<PricedItem>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let store = InventoryStore::new(pool.clone());
    let rules = load_rules(pool).await?;

    // One departure lookup per distinct trip.
    let mut departures: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for item in items {
        if departures.contains_key(&item.trip_id) {
            continue;
        }
        let departs_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT departs_at FROM trips WHERE id = $1")
                .bind(item.trip_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
        let departs_at = departs_at
            .ok_or_else(|| ApiError::NotFound(format!("trip {} not found", item.trip_id)))?;
        departures.insert(item.trip_id, departs_at);
    }

    let mut priced = Vec::with_capacity(items.len());
    for item in items {
        let quantity = item.quantity.unwrap_or(1);
        if quantity <= 0 {
            return Err(ApiError::BadRequest(format!(
                "quantity must be positive for {}",
                item.unit_code
            )));
        }
        let departs_at = departures[&item.trip_id];
        let weekday = departs_at.weekday().num_days_from_monday() as u8;
        let days_before_departure =
            (departs_at.date_naive() - Utc::now().date_naive()).num_days() as i32;

        let target = store
            .resolve_target(
                item.trip_id,
                &item.unit_code,
                &item.origin,
                &item.destination,
            )
            .await?
            .ok_or_else(|| {
                ApiError::NotFound(format!(
                    "trip {} with unit {:?}",
                    item.trip_id, item.unit_code
                ))
            })?;
        let occupancy_bp = store
            .span_occupancy_bp(item.trip_id, target.unit_id, &target.kind, target.span)
            .await?;

        // Passenger-type discounts apply to seats only; pools are
        // order-level goods.
        let passenger_type = if target.kind == "seat" {
            item.passenger_type.clone()
        } else {
            None
        };
        let input = RuleInput {
            fare_key: target.fare_key(&item.unit_code).to_string(),
            segments: target.span.segment_count(),
            quantity,
            weekday,
            days_before_departure,
            occupancy_bp,
            promo_code: promo_code.map(String::from),
            passenger_type: passenger_type.clone(),
            journey_count: context.journey_count,
            is_round_trip: context.is_round_trip,
        };
        let quote = state
            .pricing
            .price(&rules, &input)
            .map_err(|e| ApiError::BadRequest(format!("pricing {}: {e}", item.unit_code)))?;

        priced.push(PricedItem {
            trip_id: item.trip_id,
            unit_code: item.unit_code.clone(),
            origin: item.origin.clone(),
            destination: item.destination.clone(),
            from_index: target.span.from_index(),
            to_index: target.span.to_index(),
            quantity,
            passenger_type,
            quote,
        });
    }
    Ok(priced)
}
