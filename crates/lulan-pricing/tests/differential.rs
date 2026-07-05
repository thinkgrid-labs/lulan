//! Differential testing: the native engine and the reference WASM module
//! must produce IDENTICAL results — quotes and errors — for arbitrary
//! rules × inputs (the Phase 4 exit criterion).
//!
//! Builds the guest with `cargo build -p lulan-pricing-guest --target
//! wasm32-unknown-unknown --release`; skips with instructions if the
//! target isn't installed.

use std::path::PathBuf;
use std::sync::OnceLock;

use lulan_pricing::rules::{AdvanceTier, FareRuleSet, OccupancyTier, RuleInput};
use lulan_pricing::{NativeEngine, PricingEngine, PricingError, WasmEngine};
use proptest::prelude::*;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/lulan-pricing sits two levels below the workspace root")
        .to_path_buf()
}

fn wasm_engine() -> Option<&'static WasmEngine> {
    static ENGINE: OnceLock<Option<WasmEngine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let root = workspace_root();
            let status = std::process::Command::new("cargo")
                .args([
                    "build",
                    "-p",
                    "lulan-pricing-guest",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--release",
                ])
                .current_dir(&root)
                .status();
            match status {
                Ok(s) if s.success() => {}
                other => {
                    eprintln!(
                        "skipping WASM differential tests ({other:?}) — install the target with: \
                         rustup target add wasm32-unknown-unknown"
                    );
                    return None;
                }
            }
            let wasm = root.join("target/wasm32-unknown-unknown/release/lulan_pricing_guest.wasm");
            Some(WasmEngine::from_file(&wasm).expect("reference module must load"))
        })
        .as_ref()
}

/// Normalise both engines' outcomes to a comparable shape. The guest
/// stringifies EvalError with the same Display impl, so error text must
/// match too.
fn outcome(
    engine: &dyn PricingEngine,
    rules: &FareRuleSet,
    input: &RuleInput,
) -> Result<lulan_pricing::rules::Quote, String> {
    engine.price(rules, input).map_err(|e| match e {
        PricingError::Eval(err) => err.to_string(),
        PricingError::Module(err) => err,
    })
}

fn rules_strategy() -> impl Strategy<Value = FareRuleSet> {
    let fares = proptest::collection::btree_map(
        prop_oneof![
            Just("economy".to_string()),
            Just("business".to_string()),
            Just("VEHICLE_DECK".to_string()),
        ],
        0i64..200_000,
        0..3,
    );
    let occupancy = proptest::collection::vec(
        (0i64..10_000, -2_000i64..5_000).prop_map(|(min_occupancy_bp, surcharge_bp)| {
            OccupancyTier {
                min_occupancy_bp,
                surcharge_bp,
            }
        }),
        0..4,
    );
    let advance = proptest::collection::vec(
        (0i32..30, 0i64..5_000).prop_map(|(min_days, discount_bp)| AdvanceTier {
            min_days,
            discount_bp,
        }),
        0..4,
    );
    let promos = proptest::collection::btree_map(
        prop_oneof![Just("PROMO1".to_string()), Just("PROMO2".to_string())],
        0i64..10_000,
        0..2,
    );
    (
        fares,
        proptest::collection::vec(0u8..7, 0..4),
        -2_000i64..5_000,
        occupancy,
        advance,
        promos,
    )
        .prop_map(
            |(
                base_fare_per_segment,
                peak_weekdays,
                peak_surcharge_bp,
                occupancy_tiers,
                advance_purchase_tiers,
                promos,
            )| FareRuleSet {
                currency: "PHP".into(),
                base_fare_per_segment,
                peak_weekdays,
                peak_surcharge_bp,
                occupancy_tiers,
                advance_purchase_tiers,
                promos,
            },
        )
}

fn input_strategy() -> impl Strategy<Value = RuleInput> {
    (
        prop_oneof![
            Just("economy".to_string()),
            Just("business".to_string()),
            Just("VEHICLE_DECK".to_string()),
            Just("unknown_key".to_string()),
        ],
        0u8..6,       // includes invalid 0 — both engines must reject alike
        -2i32..5_000, // includes invalid quantities
        0u8..7,
        -30i32..60,
        0i64..12_000,
        prop_oneof![
            Just(None),
            Just(Some("PROMO1".to_string())),
            Just(Some("NOPE".to_string())),
        ],
    )
        .prop_map(
            |(
                fare_key,
                segments,
                quantity,
                weekday,
                days_before_departure,
                occupancy_bp,
                promo_code,
            )| RuleInput {
                fare_key,
                segments,
                quantity,
                weekday,
                days_before_departure,
                occupancy_bp,
                promo_code,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn native_and_wasm_agree_on_arbitrary_rules_and_inputs(
        rules in rules_strategy(),
        input in input_strategy(),
    ) {
        let Some(wasm) = wasm_engine() else { return Ok(()); };
        let native_result = outcome(&NativeEngine, &rules, &input);
        let wasm_result = outcome(wasm, &rules, &input);
        prop_assert_eq!(native_result, wasm_result);
    }
}

#[test]
fn garbage_module_bytes_fail_cleanly() {
    match WasmEngine::from_bytes(b"not a wasm module") {
        Err(PricingError::Module(_)) => {}
        Err(other) => panic!("expected Module error, got {other}"),
        Ok(_) => panic!("garbage bytes must not compile"),
    }
}

#[test]
fn wasm_call_latency_within_prd_target() {
    let Some(wasm) = wasm_engine() else { return };
    let rules = serde_json::from_value(serde_json::json!({
        "currency": "PHP",
        "base_fare_per_segment": {"economy": 15000},
        "peak_weekdays": [4, 5, 6],
        "peak_surcharge_bp": 1500,
        "occupancy_tiers": [{"min_occupancy_bp": 5000, "surcharge_bp": 1000}],
        "advance_purchase_tiers": [{"min_days": 7, "discount_bp": 1000}],
        "promos": {"PROMO1": 500}
    }))
    .unwrap();
    let input = RuleInput {
        fare_key: "economy".into(),
        segments: 3,
        quantity: 1,
        weekday: 5,
        days_before_departure: 10,
        occupancy_bp: 6_000,
        promo_code: Some("PROMO1".into()),
    };

    // Warm-up, then measure (instantiate-per-call included).
    for _ in 0..10 {
        wasm.price(&rules, &input).unwrap();
    }
    let mut samples: Vec<u128> = (0..200)
        .map(|_| {
            let t0 = std::time::Instant::now();
            wasm.price(&rules, &input).unwrap();
            t0.elapsed().as_micros()
        })
        .collect();
    samples.sort_unstable();
    let (p50, p95) = (samples[100], samples[190]);
    println!("wasm pricing latency: p50={p50}µs p95={p95}µs (PRD target <5000µs)");
    assert!(
        p95 < 5_000,
        "p95 {p95}µs exceeds the 5ms PRD target — investigate before shipping"
    );
}
