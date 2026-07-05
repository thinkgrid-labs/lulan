//! Pure fare rule evaluation — the shared core of every pricing engine.
//!
//! Determinism is the contract: integer-only arithmetic (minor units +
//! basis points, i128 intermediates, floor division), no clocks, no I/O,
//! no floats. The host derives time/occupancy scalars and passes them in,
//! so the same `(rules, input)` pair MUST produce the same quote in the
//! native engine, in a WASM module, or in a third-party reimplementation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A complete, serializable fare policy. Stored as JSONB in `fare_rules`
/// and shipped verbatim to pricing modules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FareRuleSet {
    /// ISO 4217 code, e.g. `PHP`.
    pub currency: String,
    /// Fare key → minor units per segment. Fare keys are seat fare classes
    /// (`economy`) or pool unit codes (`VEHICLE_DECK`, `CARGO_KG`).
    pub base_fare_per_segment: BTreeMap<String, i64>,
    /// Weekdays (0 = Monday … 6 = Sunday) that attract the peak surcharge.
    #[serde(default)]
    pub peak_weekdays: Vec<u8>,
    /// Basis points added to base on peak weekdays.
    #[serde(default)]
    pub peak_surcharge_bp: i64,
    /// Demand pricing: the highest tier whose threshold is reached applies.
    #[serde(default)]
    pub occupancy_tiers: Vec<OccupancyTier>,
    /// Early-bird discounts: the highest tier whose min_days is reached.
    #[serde(default)]
    pub advance_purchase_tiers: Vec<AdvanceTier>,
    /// Promo code → discount basis points (applied to base).
    #[serde(default)]
    pub promos: BTreeMap<String, i64>,
    /// Passenger type (`child`, `senior`, `pwd`, …) → discount basis
    /// points. Senior/PWD discounts are legally mandated in some markets
    /// (e.g. PH), so this is a first-class fare input.
    #[serde(default)]
    pub passenger_type_discounts: BTreeMap<String, i64>,
    /// Discount applied to every item of a round-trip itinerary, basis
    /// points.
    #[serde(default)]
    pub round_trip_discount_bp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccupancyTier {
    /// Applies when occupancy (basis points, 0–10000) is at least this.
    pub min_occupancy_bp: i64,
    pub surcharge_bp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvanceTier {
    /// Applies when booking at least this many days before departure.
    pub min_days: i32,
    pub discount_bp: i64,
}

/// Host-derived scalars for one priced line item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleInput {
    /// Fare key (seat fare class or pool unit code).
    pub fare_key: String,
    /// Segments covered by the journey span (≥ 1).
    pub segments: u8,
    /// Seats: 1. Pools: reserved quantity (kg, slots, …).
    pub quantity: i32,
    /// Departure weekday, 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    /// Whole days between purchase and departure (may be ≤ 0).
    pub days_before_departure: i32,
    /// Current occupancy of the fare key on this span, basis points.
    pub occupancy_bp: i64,
    #[serde(default)]
    pub promo_code: Option<String>,
    /// The travelling passenger's type for seat items (`adult`, `child`,
    /// `senior`, `pwd`, `infant`); None for order-level pool items.
    #[serde(default)]
    pub passenger_type: Option<String>,
    /// Journeys in the itinerary this item belongs to (1 = one-way).
    /// Host-derived so modules stay deterministic.
    #[serde(default = "default_journey_count")]
    pub journey_count: u32,
    /// True when the itinerary is an out-and-back pairing (host-derived:
    /// exactly two journeys, the second reversing the first's O&D).
    #[serde(default)]
    pub is_round_trip: bool,
}

fn default_journey_count() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adjustment {
    pub label: String,
    pub amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub currency: String,
    pub base_minor: i64,
    pub adjustments: Vec<Adjustment>,
    pub total_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum EvalError {
    #[error("no base fare configured for fare key {0:?}")]
    UnknownFareKey(String),
    #[error("segments must be at least 1")]
    NoSegments,
    #[error("quantity must be positive, got {0}")]
    NonPositiveQuantity(i32),
}

/// The wire request a pricing module receives (JSON-encoded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceRequest {
    pub rules: FareRuleSet,
    pub input: RuleInput,
}

/// The wire response a pricing module returns (JSON-encoded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Quote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

/// Floor-division basis-point application via i128 — overflow-proof for
/// any realistic fare, and bit-identical on every target.
fn bp(amount: i64, basis_points: i64) -> i64 {
    ((amount as i128 * basis_points as i128) / 10_000) as i64
}

/// Evaluate one line item. Pure function; see module docs for the
/// determinism contract.
pub fn evaluate(rules: &FareRuleSet, input: &RuleInput) -> Result<Quote, EvalError> {
    if input.segments == 0 {
        return Err(EvalError::NoSegments);
    }
    if input.quantity <= 0 {
        return Err(EvalError::NonPositiveQuantity(input.quantity));
    }
    let per_segment = *rules
        .base_fare_per_segment
        .get(&input.fare_key)
        .ok_or_else(|| EvalError::UnknownFareKey(input.fare_key.clone()))?;

    let base = per_segment * input.segments as i64 * input.quantity as i64;
    let mut adjustments = Vec::new();

    if rules.peak_weekdays.contains(&input.weekday) && rules.peak_surcharge_bp != 0 {
        adjustments.push(Adjustment {
            label: "peak_weekday".into(),
            amount_minor: bp(base, rules.peak_surcharge_bp),
        });
    }

    // Highest tier whose threshold is met wins (evaluation order must not
    // depend on how the operator sorted the JSON).
    if let Some(tier) = rules
        .occupancy_tiers
        .iter()
        .filter(|t| input.occupancy_bp >= t.min_occupancy_bp)
        .max_by_key(|t| t.min_occupancy_bp)
        && tier.surcharge_bp != 0
    {
        adjustments.push(Adjustment {
            label: "occupancy".into(),
            amount_minor: bp(base, tier.surcharge_bp),
        });
    }

    if let Some(tier) = rules
        .advance_purchase_tiers
        .iter()
        .filter(|t| input.days_before_departure >= t.min_days)
        .max_by_key(|t| t.min_days)
        && tier.discount_bp != 0
    {
        adjustments.push(Adjustment {
            label: "advance_purchase".into(),
            amount_minor: -bp(base, tier.discount_bp),
        });
    }

    if let Some(passenger_type) = &input.passenger_type
        && let Some(discount_bp) = rules.passenger_type_discounts.get(passenger_type)
        && *discount_bp != 0
    {
        adjustments.push(Adjustment {
            label: format!("passenger:{passenger_type}"),
            amount_minor: -bp(base, *discount_bp),
        });
    }

    if input.is_round_trip && rules.round_trip_discount_bp != 0 {
        adjustments.push(Adjustment {
            label: "round_trip".into(),
            amount_minor: -bp(base, rules.round_trip_discount_bp),
        });
    }

    if let Some(code) = &input.promo_code
        && let Some(discount_bp) = rules.promos.get(code)
    {
        adjustments.push(Adjustment {
            label: format!("promo:{code}"),
            amount_minor: -bp(base, *discount_bp),
        });
    }

    let total = (base + adjustments.iter().map(|a| a.amount_minor).sum::<i64>()).max(0);

    Ok(Quote {
        currency: rules.currency.clone(),
        base_minor: base,
        adjustments,
        total_minor: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn demo_rules() -> FareRuleSet {
        FareRuleSet {
            currency: "PHP".into(),
            base_fare_per_segment: BTreeMap::from([
                ("economy".into(), 15_000),
                ("business".into(), 30_000),
                ("VEHICLE_DECK".into(), 100_000),
                ("CARGO_KG".into(), 500),
            ]),
            peak_weekdays: vec![4, 5, 6],
            peak_surcharge_bp: 1_500,
            occupancy_tiers: vec![
                OccupancyTier {
                    min_occupancy_bp: 5_000,
                    surcharge_bp: 1_000,
                },
                OccupancyTier {
                    min_occupancy_bp: 8_000,
                    surcharge_bp: 2_500,
                },
            ],
            advance_purchase_tiers: vec![
                AdvanceTier {
                    min_days: 7,
                    discount_bp: 1_000,
                },
                AdvanceTier {
                    min_days: 14,
                    discount_bp: 2_000,
                },
            ],
            promos: BTreeMap::from([("BAGONGBYAHE".into(), 500)]),
            passenger_type_discounts: BTreeMap::from([
                ("senior".into(), 2_000),
                ("pwd".into(), 2_000),
                ("child".into(), 5_000),
            ]),
            round_trip_discount_bp: 1_000,
        }
    }

    fn input(fare_key: &str) -> RuleInput {
        RuleInput {
            fare_key: fare_key.into(),
            segments: 3,
            quantity: 1,
            weekday: 1,
            days_before_departure: 0,
            occupancy_bp: 0,
            promo_code: None,
            passenger_type: None,
            journey_count: 1,
            is_round_trip: false,
        }
    }

    #[test]
    fn base_is_per_segment_times_quantity() {
        let q = evaluate(&demo_rules(), &input("economy")).unwrap();
        assert_eq!(q.base_minor, 45_000);
        assert_eq!(q.total_minor, 45_000);
        assert!(q.adjustments.is_empty());

        let mut cargo = input("CARGO_KG");
        cargo.quantity = 250;
        let q = evaluate(&demo_rules(), &cargo).unwrap();
        assert_eq!(q.base_minor, 500 * 3 * 250);
    }

    #[test]
    fn highest_matching_occupancy_tier_wins_regardless_of_order() {
        let mut rules = demo_rules();
        rules.occupancy_tiers.reverse();
        let mut i = input("economy");
        i.occupancy_bp = 9_000;
        let q = evaluate(&rules, &i).unwrap();
        // 45000 * 25% = 11250, not the 10% tier.
        assert_eq!(q.adjustments[0].amount_minor, 11_250);
    }

    #[test]
    fn surcharges_and_discounts_stack_on_base() {
        let mut i = input("economy");
        i.weekday = 5; // peak
        i.occupancy_bp = 5_000; // +10%
        i.days_before_departure = 20; // -20% (14-day tier)
        i.promo_code = Some("BAGONGBYAHE".into()); // -5%
        let q = evaluate(&demo_rules(), &i).unwrap();
        // 45000 + 6750 + 4500 - 9000 - 2250 = 45000
        assert_eq!(q.total_minor, 45_000);
        assert_eq!(q.adjustments.len(), 4);
    }

    #[test]
    fn total_never_goes_negative_and_unknown_promo_is_ignored() {
        let mut rules = demo_rules();
        rules.advance_purchase_tiers = vec![AdvanceTier {
            min_days: 0,
            discount_bp: 20_000, // absurd -200%
        }];
        let mut i = input("economy");
        i.promo_code = Some("NOT_A_CODE".into());
        let q = evaluate(&rules, &i).unwrap();
        assert_eq!(q.total_minor, 0);
        assert_eq!(q.adjustments.len(), 1, "unknown promo adds nothing");
    }

    #[test]
    fn passenger_type_discounts_apply_and_unknown_types_do_not() {
        let mut i = input("economy");
        i.passenger_type = Some("senior".into());
        let q = evaluate(&demo_rules(), &i).unwrap();
        // 45000 - 20% = 36000
        assert_eq!(q.total_minor, 36_000);
        assert_eq!(q.adjustments[0].label, "passenger:senior");

        i.passenger_type = Some("astronaut".into());
        let q = evaluate(&demo_rules(), &i).unwrap();
        assert_eq!(q.total_minor, 45_000);
        assert!(q.adjustments.is_empty());
    }

    #[test]
    fn round_trip_discount_applies_only_to_round_trips() {
        let mut i = input("economy");
        i.journey_count = 2;
        i.is_round_trip = true;
        let q = evaluate(&demo_rules(), &i).unwrap();
        // 45000 - 10% = 40500
        assert_eq!(q.total_minor, 40_500);
        assert_eq!(q.adjustments[0].label, "round_trip");

        // Two journeys that don't pair (multi-city) get no discount.
        i.is_round_trip = false;
        let q = evaluate(&demo_rules(), &i).unwrap();
        assert_eq!(q.total_minor, 45_000);
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        assert_eq!(
            evaluate(&demo_rules(), &input("first")),
            Err(EvalError::UnknownFareKey("first".into()))
        );
        let mut i = input("economy");
        i.segments = 0;
        assert_eq!(evaluate(&demo_rules(), &i), Err(EvalError::NoSegments));
        let mut i = input("economy");
        i.quantity = 0;
        assert_eq!(
            evaluate(&demo_rules(), &i),
            Err(EvalError::NonPositiveQuantity(0))
        );
    }
}
