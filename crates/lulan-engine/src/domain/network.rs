//! The transit network: locations, routes, and trips.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{LocationId, ResourceId, RouteId, TripId, TripPatternId};
use super::segment::{SegmentSpan, SpanError};

/// A stop, port, terminal, or airport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub id: LocationId,
    /// Short unique code, e.g. `BTG`, `CEB`.
    pub code: String,
    pub name: String,
    /// IANA timezone name, e.g. `Asia/Manila`.
    pub timezone: String,
}

/// An ordered sequence of locations. A route with N stops has N−1 segments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: RouteId,
    pub code: String,
    pub name: String,
    /// Ordered stops; index in this list is the stop index.
    pub stops: Vec<LocationId>,
}

impl Route {
    pub fn segment_count(&self) -> u8 {
        (self.stops.len().saturating_sub(1)) as u8
    }

    /// The segment span a passenger occupies travelling from `origin` to
    /// `destination`. `None` if either stop is not on the route or they are
    /// out of order.
    pub fn span_between(
        &self,
        origin: LocationId,
        destination: LocationId,
    ) -> Option<Result<SegmentSpan, SpanError>> {
        let from = self.stops.iter().position(|s| *s == origin)?;
        let to = self.stops.iter().position(|s| *s == destination)?;
        if from >= to {
            return None;
        }
        Some(SegmentSpan::new(from as u8, to as u8))
    }
}

/// A recurring schedule template. Trip generation from patterns lands in a
/// later phase; the type exists so trips can already reference their origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripPattern {
    pub id: TripPatternId,
    pub route_id: RouteId,
    pub resource_id: ResourceId,
    pub departure_time: NaiveTime,
    /// Bit i set = runs on weekday i (0 = Monday).
    pub days_of_week: u8,
}

/// A dated, sellable instance of a route run by a specific resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trip {
    pub id: TripId,
    pub route_id: RouteId,
    pub resource_id: ResourceId,
    pub service_date: NaiveDate,
    pub departs_at: DateTime<Utc>,
    pub segment_count: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_between_maps_stop_positions_to_segments() {
        let stops: Vec<LocationId> = (0..4).map(|_| LocationId::new()).collect();
        let route = Route {
            id: RouteId::new(),
            code: "R1".into(),
            name: "Test".into(),
            stops: stops.clone(),
        };
        assert_eq!(route.segment_count(), 3);

        let span = route.span_between(stops[1], stops[3]).unwrap().unwrap();
        assert_eq!((span.from_index(), span.to_index()), (1, 3));

        // Reversed or unknown stops are not journeys.
        assert!(route.span_between(stops[3], stops[1]).is_none());
        assert!(route.span_between(stops[0], LocationId::new()).is_none());
    }
}
