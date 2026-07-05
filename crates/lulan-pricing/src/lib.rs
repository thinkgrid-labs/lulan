//! Pricing engine interface (Phase 4).
//!
//! The stable contract is [`PricingEngine`]; implementations planned:
//! a native rule engine and an in-process WASM (wasmtime) host.
//! Money is integer minor units — never floats.

use serde::{Deserialize, Serialize};

/// An amount in minor units (e.g. centavos) with an ISO 4217 currency code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub minor_units: i64,
    pub currency: String,
}
