//! Offline ticket verification core (Phase 5).
//!
//! Pure verification logic — Ed25519 signature check, validity window,
//! trip match — with no server dependency. Compiles to WASM for the
//! MIT-licensed `@lulan/validate` package (browser + React Native) and is
//! usable natively. This crate is deliberately MIT (not AGPL) so proprietary
//! conductor apps can embed it; keep dependencies minimal.
