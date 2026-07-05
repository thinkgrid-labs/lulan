//! Pure domain types and invariants. No I/O in this module tree.

mod capacity;
mod ids;
mod network;
mod pool;
mod segment;

pub use capacity::{CapacityUnit, CapacityUnitKind, Resource, ResourceKind};
pub use ids::{
    CapacityUnitId, HoldId, LocationId, OrderId, ResourceId, RouteId, TicketId, TripId,
    TripPatternId,
};
pub use network::{Location, Route, Trip, TripPattern};
pub use pool::{PoolError, PoolOccupancy};
pub use segment::{MAX_SEGMENTS, SegmentSpan, SpanError};
