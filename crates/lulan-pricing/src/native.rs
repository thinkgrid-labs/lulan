//! In-process rule engine — the default and the differential-testing
//! reference for WASM modules.

use crate::rules::{FareRuleSet, Quote, RuleInput, evaluate};
use crate::{PricingEngine, PricingError};

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeEngine;

impl PricingEngine for NativeEngine {
    fn price(&self, rules: &FareRuleSet, input: &RuleInput) -> Result<Quote, PricingError> {
        Ok(evaluate(rules, input)?)
    }
}
