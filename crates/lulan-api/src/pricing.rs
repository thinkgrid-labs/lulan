//! Pricing glue: loads the active fare rules, derives the pure-engine
//! scalars (weekday, advance days, occupancy) from live data, and prices
//! line items through whichever [`PricingEngine`] the server booted with.

use chrono::{DateTime, Datelike, Utc};
use lulan_engine::inventory::InventoryStore;
use lulan_pricing::rules::{FareRuleSet, Quote, RuleInput};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// One item to price, as clients describe it.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceableItem {
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    #[serde(default)]
    pub quantity: Option<i32>,
    /// Travelling passenger's type for seat items (drives mandated
    /// discounts); ignored for pools.
    #[serde(default)]
    pub passenger_type: Option<String>,
}

/// A priced line item, span-resolved.
pub struct PricedItem {
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub from_index: u8,
    pub to_index: u8,
    pub quantity: i32,
    pub passenger_type: Option<String>,
    pub quote: Quote,
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

/// Price every item of a prospective order against live occupancy.
pub async fn price_items(
    state: &AppState,
    trip_id: Uuid,
    items: &[PriceableItem],
    promo_code: Option<&str>,
) -> Result<Vec<PricedItem>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let store = InventoryStore::new(pool.clone());
    let rules = load_rules(pool).await?;

    let departs_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT departs_at FROM trips WHERE id = $1")
            .bind(trip_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    let departs_at =
        departs_at.ok_or_else(|| ApiError::NotFound(format!("trip {trip_id} not found")))?;
    let weekday = departs_at.weekday().num_days_from_monday() as u8;
    let days_before_departure =
        (departs_at.date_naive() - Utc::now().date_naive()).num_days() as i32;

    let mut priced = Vec::with_capacity(items.len());
    for item in items {
        let quantity = item.quantity.unwrap_or(1);
        if quantity <= 0 {
            return Err(ApiError::BadRequest(format!(
                "quantity must be positive for {}",
                item.unit_code
            )));
        }
        let target = store
            .resolve_target(trip_id, &item.unit_code, &item.origin, &item.destination)
            .await?
            .ok_or_else(|| {
                ApiError::NotFound(format!("trip {trip_id} with unit {:?}", item.unit_code))
            })?;
        let occupancy_bp = store
            .span_occupancy_bp(trip_id, target.unit_id, &target.kind, target.span)
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
        };
        let quote = state
            .pricing
            .price(&rules, &input)
            .map_err(|e| ApiError::BadRequest(format!("pricing {}: {e}", item.unit_code)))?;

        priced.push(PricedItem {
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
