//! Lulan pricing (ADR 0003): the stable contract is [`PricingEngine`];
//! runtimes are interchangeable.
//!
//! - [`rules`] — the pure, deterministic evaluation core and wire types.
//!   Always available; compiles to any target including wasm32 (this is
//!   what pricing modules link against, `default-features = false`).
//! - [`NativeEngine`] — evaluates rules in-process (`host` feature).
//! - [`WasmEngine`] — runs an operator-supplied WASM module under
//!   wasmtime, fuel-metered and memory-capped (`host` feature).
//!
//! The module ABI is defined in `wit/pricing.wit`.

pub mod rules;

#[cfg(feature = "host")]
mod native;
#[cfg(feature = "host")]
mod wasm;

#[cfg(feature = "host")]
pub use native::NativeEngine;
#[cfg(feature = "host")]
pub use wasm::WasmEngine;

use rules::{EvalError, FareRuleSet, Quote, RuleInput};

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error(transparent)]
    Eval(#[from] EvalError),
    #[error("pricing module error: {0}")]
    Module(String),
}

/// One line item in, one quote out. Synchronous by design: pricing is a
/// bounded CPU-only computation (the <5 ms PRD target), never I/O.
pub trait PricingEngine: Send + Sync {
    fn price(&self, rules: &FareRuleSet, input: &RuleInput) -> Result<Quote, PricingError>;
}
