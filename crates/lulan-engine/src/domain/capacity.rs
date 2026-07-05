//! Resources and their reservable capacity units (ADR 0005).

use serde::{Deserialize, Serialize};

use super::ids::{CapacityUnitId, ResourceId};

/// A vehicle, vessel, or aircraft with a fixed layout of capacity units.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub code: String,
    pub name: String,
    pub kind: ResourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Bus,
    Ferry,
    Aircraft,
    Other,
}

/// The generic reservable thing.
///
/// - `Seat`: identity-based — a specific physical place; sold at most once
///   per segment. Occupancy is a segment bitmask.
/// - `Pool`: count-based — cargo kilograms, vehicle deck slots, meals,
///   standing room. Occupancy is a per-segment remaining counter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityUnit {
    pub id: CapacityUnitId,
    pub resource_id: ResourceId,
    /// Seat code (`12A`) or pool code (`VEHICLE_DECK`, `CARGO_KG`).
    pub code: String,
    pub kind: CapacityUnitKind,
    /// Required for seats; optional for pools.
    pub fare_class: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityUnitKind {
    Seat,
    /// Pool with the given per-segment capacity.
    Pool {
        capacity: i32,
    },
}
