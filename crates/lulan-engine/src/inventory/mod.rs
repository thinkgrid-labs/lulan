//! Segment inventory: availability reads, Redis soft holds, and Postgres
//! hard claims (Phases 1–2). See ADR 0002 for the two-tier design.

mod holds;
mod store;

pub use holds::{Hold, HoldError, HoldStore};
pub use store::{
    ClaimOutcome, ClaimTarget, FareAvailability, InventoryStore, PoolAvailability,
    SeatAvailability, StoreError, TripAvailability, TripSummary,
};
