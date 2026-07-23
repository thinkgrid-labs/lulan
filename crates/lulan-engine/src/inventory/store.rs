//! Read-side inventory queries. Availability is computed in SQL with the
//! same mask/min-over-span algebra as the domain types, so the database
//! answer and the in-memory answer can never drift.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::{SegmentSpan, SpanError};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("unknown location code {0:?} for this trip's route")]
    UnknownStop(String),
    #[error("{origin:?} does not precede {destination:?} on this trip's route")]
    StopsOutOfOrder { origin: String, destination: String },
    #[error("this trip is {status} and no longer open for sale")]
    TripNotSellable { status: String },
    #[error("this trip left {origin} at {departed_at} and can no longer be booked")]
    TripDeparted {
        origin: String,
        departed_at: DateTime<Utc>,
    },
    #[error("invalid segment span: {0}")]
    Span(#[from] SpanError),
    /// A stored event stream does not replay through the state machine.
    /// Only reachable if the log and the code disagree — surfaced rather
    /// than panicked so one bad stream cannot take the process down.
    #[error("event stream {stream_id} is unreplayable at seq {stream_seq}: {detail}")]
    UnreplayableStream {
        stream_id: uuid::Uuid,
        stream_seq: i32,
        detail: String,
    },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Seat availability grouped by fare class for one trip + span.
#[derive(Debug, Serialize)]
pub struct FareAvailability {
    pub fare_class: String,
    pub available: i64,
    pub total: i64,
}

/// Remaining pool capacity for one trip + span.
#[derive(Debug, Serialize)]
pub struct PoolAvailability {
    pub code: String,
    pub remaining: i32,
}

#[derive(Debug, Serialize)]
pub struct SeatAvailability {
    #[serde(skip)]
    pub unit_id: Uuid,
    pub code: String,
    pub fare_class: String,
    /// Free to CLAIM on this span (sold-state only).
    pub available: bool,
    /// Another session currently soft-holds an overlapping span. Advisory
    /// (Redis) — enriched by the API layer; false when holds are down.
    pub held: bool,
}

/// The carrier/agency operating a trip (airline, ferry line, bus company).
#[derive(Debug, Serialize)]
pub struct Operator {
    pub code: String,
    pub name: String,
}

/// The physical vehicle serving a trip (aircraft, vessel, coach).
#[derive(Debug, Serialize)]
pub struct Vehicle {
    pub code: String,
    pub name: String,
    pub kind: String,
}

/// A search hit: one trip serving the requested origin → destination, with
/// schedule (departure + arrival for the requested span) and identity.
#[derive(Debug, Serialize)]
pub struct TripSummary {
    pub trip_id: Uuid,
    pub route_code: String,
    /// Carrier; None until an operator is assigned to the trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<Operator>,
    /// Passenger-facing service designator (flight/service number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_number: Option<String>,
    pub vehicle: Vehicle,
    pub origin: String,
    pub destination: String,
    /// Departure from the requested origin (not necessarily the route's
    /// first stop).
    pub departs_at: DateTime<Utc>,
    /// Arrival at the requested destination, when the schedule is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrives_at: Option<DateTime<Utc>>,
    /// Journey time for the requested span, when the schedule is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
    pub from_index: u8,
    pub to_index: u8,
    pub seats: Vec<FareAvailability>,
    pub pools: Vec<PoolAvailability>,
}

/// Per-unit availability for one trip + span.
#[derive(Debug, Serialize)]
pub struct TripAvailability {
    pub trip_id: Uuid,
    pub origin: String,
    pub destination: String,
    pub from_index: u8,
    pub to_index: u8,
    pub seats: Vec<SeatAvailability>,
    pub pools: Vec<PoolAvailability>,
}

/// Result of a claim attempt against the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The guarded update won: inventory is now yours.
    Claimed,
    /// Another claim already covers part of the span (or pool quantity).
    Conflict,
    /// Trip/unit combination does not exist.
    NotFound,
}

/// What a claim or hold request resolves to.
#[derive(Debug)]
pub struct ClaimTarget {
    pub unit_id: Uuid,
    /// `"seat"` or `"pool"` (matches the capacity_units CHECK constraint).
    pub kind: String,
    /// Set for seats; None for pools.
    pub fare_class: Option<String>,
    pub span: SegmentSpan,
}

impl ClaimTarget {
    /// The pricing lookup key: a seat's fare class, or the pool's own code.
    pub fn fare_key<'a>(&'a self, unit_code: &'a str) -> &'a str {
        self.fare_class.as_deref().unwrap_or(unit_code)
    }
}

#[derive(Clone)]
pub struct InventoryStore {
    pool: PgPool,
}

impl InventoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve (trip, unit code, origin, destination) to a claimable target.
    /// `Ok(None)` if the trip doesn't exist or the unit isn't on its vessel.
    ///
    /// This is the one gate every sale passes through — holds, claims,
    /// quotes and orders all resolve here — so it is also where a trip
    /// stops being sellable. Search already hides cancelled and past
    /// departures, but a trip id is enough to bypass search, and selling a
    /// seat on a departure that has left is worse than a 404.
    pub async fn resolve_target(
        &self,
        trip_id: Uuid,
        unit_code: &str,
        origin: &str,
        destination: &str,
    ) -> Result<Option<ClaimTarget>, StoreError> {
        let Some(row) = sqlx::query(
            r#"
            SELECT cu.id AS unit_id, cu.kind, cu.fare_class,
                   t.route_id, t.departs_at, t.status
            FROM trips t
            JOIN capacity_units cu
              ON cu.resource_id = t.resource_id AND cu.code = $2
            WHERE t.id = $1
            "#,
        )
        .bind(trip_id)
        .bind(unit_code)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        let status: String = row.try_get("status")?;
        if status != "scheduled" {
            return Err(StoreError::TripNotSellable { status });
        }

        let route_id: Uuid = row.try_get("route_id")?;
        let (span, depart_offset) = self.resolve_span(route_id, origin, destination).await?;

        // Judged at the requested ORIGIN, not the route's first stop: a
        // trip mid-journey can still sell its later legs.
        let departs_at: DateTime<Utc> = row.try_get("departs_at")?;
        let boards_at = departs_at + chrono::Duration::minutes(i64::from(depart_offset));
        if boards_at <= Utc::now() {
            return Err(StoreError::TripDeparted {
                origin: origin.to_string(),
                departed_at: boards_at,
            });
        }

        Ok(Some(ClaimTarget {
            unit_id: row.try_get("unit_id")?,
            kind: row.try_get("kind")?,
            fare_class: row.try_get("fare_class")?,
            span,
        }))
    }

    /// How sold this unit's fare bucket is for the span, in basis points
    /// (0–10000) — the demand input for occupancy pricing. Seats measure
    /// their fare class across the vessel; pools measure their own fill.
    pub async fn span_occupancy_bp(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
        kind: &str,
        span: SegmentSpan,
    ) -> Result<i64, StoreError> {
        if kind == "seat" {
            let row = sqlx::query(
                r#"
                SELECT count(*) FILTER (WHERE (so.occupied_mask & $3) <> 0) AS sold,
                       count(*) AS total
                FROM seat_occupancy so
                JOIN capacity_units cu ON cu.id = so.unit_id
                WHERE so.trip_id = $1
                  AND cu.fare_class = (SELECT fare_class FROM capacity_units WHERE id = $2)
                "#,
            )
            .bind(trip_id)
            .bind(unit_id)
            .bind(span.mask() as i64)
            .fetch_one(&self.pool)
            .await?;
            let sold: i64 = row.try_get("sold")?;
            let total: i64 = row.try_get("total")?;
            Ok(if total == 0 { 0 } else { sold * 10_000 / total })
        } else {
            let Some(row) = sqlx::query(
                r#"
                SELECT (SELECT min(x) FROM unnest(po.remaining[$3:$4]) AS x) AS remaining,
                       cu.pool_capacity
                FROM pool_occupancy po
                JOIN capacity_units cu ON cu.id = po.unit_id
                WHERE po.trip_id = $1 AND po.unit_id = $2
                "#,
            )
            .bind(trip_id)
            .bind(unit_id)
            .bind(i32::from(span.from_index()) + 1)
            .bind(i32::from(span.to_index()))
            .fetch_optional(&self.pool)
            .await?
            else {
                return Ok(0);
            };
            let remaining: i32 = row.try_get::<Option<i32>, _>("remaining")?.unwrap_or(0);
            let capacity: i32 = row.try_get::<Option<i32>, _>("pool_capacity")?.unwrap_or(0);
            Ok(if capacity <= 0 {
                0
            } else {
                (capacity - remaining) as i64 * 10_000 / capacity as i64
            })
        }
    }

    /// How many seats this trip has. The denominator for the per-trip
    /// hold ceiling.
    pub async fn seat_count(&self, trip_id: Uuid) -> Result<i64, StoreError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM seat_occupancy so
             JOIN capacity_units cu ON cu.id = so.unit_id
             WHERE so.trip_id = $1 AND cu.kind = 'seat'",
        )
        .bind(trip_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Current occupancy mask for one seat, reinterpreted as u64.
    pub async fn seat_mask(&self, trip_id: Uuid, unit_id: Uuid) -> Result<Option<u64>, StoreError> {
        let mask = sqlx::query_scalar::<_, i64>(
            "SELECT occupied_mask FROM seat_occupancy WHERE trip_id = $1 AND unit_id = $2",
        )
        .bind(trip_id)
        .bind(unit_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(mask.map(|m| m as u64))
    }

    /// The hard claim (ADR 0002): a single guarded UPDATE. Zero rows
    /// affected means someone else owns part of the span — never partial,
    /// never lost. Postgres cannot double-sell a segment even if every
    /// layer above is wrong.
    pub async fn claim_seat(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
        span: SegmentSpan,
    ) -> Result<ClaimOutcome, StoreError> {
        if claim_seat_exec(&self.pool, trip_id, unit_id, span).await? == 1 {
            return Ok(ClaimOutcome::Claimed);
        }
        self.disambiguate_seat(trip_id, unit_id).await
    }

    /// Release exactly a previously claimed span. Guarded on owning every
    /// bit, so a stray release can't free someone else's segments.
    pub async fn release_seat(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
        span: SegmentSpan,
    ) -> Result<ClaimOutcome, StoreError> {
        if release_seat_exec(&self.pool, trip_id, unit_id, span).await? == 1 {
            return Ok(ClaimOutcome::Claimed);
        }
        self.disambiguate_seat(trip_id, unit_id).await
    }

    async fn disambiguate_seat(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
    ) -> Result<ClaimOutcome, StoreError> {
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM seat_occupancy WHERE trip_id = $1 AND unit_id = $2",
        )
        .bind(trip_id)
        .bind(unit_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(if exists > 0 {
            ClaimOutcome::Conflict
        } else {
            ClaimOutcome::NotFound
        })
    }

    /// Pool claim: subtract `qty` from every segment in the span, guarded on
    /// the span's minimum remaining. The `0 <= ALL(remaining)` CHECK in the
    /// schema backs this up at the database level.
    pub async fn claim_pool(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
        span: SegmentSpan,
        qty: i32,
    ) -> Result<ClaimOutcome, StoreError> {
        if qty <= 0 {
            return Ok(ClaimOutcome::Conflict);
        }
        if claim_pool_exec(&self.pool, trip_id, unit_id, span, qty).await? == 1 {
            return Ok(ClaimOutcome::Claimed);
        }
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pool_occupancy WHERE trip_id = $1 AND unit_id = $2",
        )
        .bind(trip_id)
        .bind(unit_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(if exists > 0 {
            ClaimOutcome::Conflict
        } else {
            ClaimOutcome::NotFound
        })
    }

    /// Trips serving `origin → destination` on `date`, with availability
    /// summarised per fare class for exactly that segment span.
    pub async fn search_trips(
        &self,
        origin: &str,
        destination: &str,
        date: NaiveDate,
        limit: i64,
    ) -> Result<Vec<TripSummary>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT t.id, r.code AS route_code, t.departs_at, t.service_number,
                   o.stop_index AS from_index, d.stop_index AS to_index,
                   o.depart_offset_min AS depart_offset, d.arrive_offset_min AS arrive_offset,
                   res.code AS vehicle_code, res.name AS vehicle_name, res.kind AS vehicle_kind,
                   op.code AS operator_code, op.name AS operator_name
            FROM trips t
            JOIN routes r        ON r.id = t.route_id
            JOIN resources res   ON res.id = t.resource_id
            LEFT JOIN operators op ON op.id = t.operator_id
            JOIN route_stops o   ON o.route_id = r.id
            JOIN locations lo    ON lo.id = o.location_id AND lo.code = $1
            JOIN route_stops d   ON d.route_id = r.id
            JOIN locations ld    ON ld.id = d.location_id AND ld.code = $2
            WHERE o.stop_index < d.stop_index
              AND t.service_date = $3
              AND t.status = 'scheduled'
            ORDER BY t.departs_at
            LIMIT $4
            "#,
        )
        .bind(origin)
        .bind(destination)
        .bind(date)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut trips = Vec::with_capacity(rows.len());
        // (trip, mask) for the batched availability queries below. Spans
        // differ per trip: two departures serving the same city pair can
        // run different routes, so the stop indices are not shared.
        let mut spans: Vec<(Uuid, i64)> = Vec::with_capacity(rows.len());
        for row in rows {
            let trip_id: Uuid = row.try_get("id")?;
            let from_index: i16 = row.try_get("from_index")?;
            let to_index: i16 = row.try_get("to_index")?;
            let span = SegmentSpan::new(from_index as u8, to_index as u8)?;
            let departs_at: DateTime<Utc> = row.try_get("departs_at")?;

            // Absolute schedule for the requested span from route-pattern
            // offsets. A route with no schedule has all-zero offsets, which
            // we surface as "unknown" (None) rather than a bogus 0-minute
            // trip.
            let depart_offset: i32 = row.try_get("depart_offset")?;
            let arrive_offset: i32 = row.try_get("arrive_offset")?;
            let (arrives_at, duration_minutes) = if arrive_offset > depart_offset {
                let arrives = departs_at + chrono::Duration::minutes(arrive_offset as i64);
                let departs = departs_at + chrono::Duration::minutes(depart_offset as i64);
                (Some(arrives), Some((arrives - departs).num_minutes()))
            } else {
                (None, None)
            };
            // Origin departure = trip departure + this stop's depart offset.
            let leg_departs_at = departs_at + chrono::Duration::minutes(depart_offset as i64);

            let operator = match (
                row.try_get::<Option<String>, _>("operator_code")?,
                row.try_get::<Option<String>, _>("operator_name")?,
            ) {
                (Some(code), Some(name)) => Some(Operator { code, name }),
                _ => None,
            };

            trips.push(TripSummary {
                trip_id,
                route_code: row.try_get("route_code")?,
                operator,
                service_number: row.try_get("service_number")?,
                vehicle: Vehicle {
                    code: row.try_get("vehicle_code")?,
                    name: row.try_get("vehicle_name")?,
                    kind: row.try_get("vehicle_kind")?,
                },
                origin: origin.to_string(),
                destination: destination.to_string(),
                departs_at: leg_departs_at,
                arrives_at,
                duration_minutes,
                from_index: span.from_index(),
                to_index: span.to_index(),
                seats: Vec::new(),
                pools: Vec::new(),
            });
            spans.push((trip_id, span.mask() as i64));
        }

        // Two set-based queries for the whole page, rather than two per
        // trip. Search is unauthenticated and the most database-expensive
        // endpoint in the API; 2N+1 queries on it was the DoS surface.
        let mut seats = self.fare_availability_batch(&spans).await?;
        let mut pools = self.pool_availability_batch(&spans).await?;
        for trip in &mut trips {
            trip.seats = seats.remove(&trip.trip_id).unwrap_or_default();
            trip.pools = pools.remove(&trip.trip_id).unwrap_or_default();
        }
        Ok(trips)
    }

    /// Fare-class availability for many (trip, span) pairs at once.
    async fn fare_availability_batch(
        &self,
        spans: &[(Uuid, i64)],
    ) -> Result<std::collections::HashMap<Uuid, Vec<FareAvailability>>, StoreError> {
        let (trip_ids, masks): (Vec<Uuid>, Vec<i64>) = spans.iter().copied().unzip();
        let rows = sqlx::query(
            r#"
            SELECT q.trip_id, cu.fare_class,
                   count(*) FILTER (WHERE (so.occupied_mask & q.mask) = 0) AS available,
                   count(*) AS total
            FROM unnest($1::uuid[], $2::bigint[]) AS q(trip_id, mask)
            JOIN seat_occupancy so ON so.trip_id = q.trip_id
            JOIN capacity_units cu ON cu.id = so.unit_id
            GROUP BY q.trip_id, cu.fare_class
            ORDER BY q.trip_id, cu.fare_class
            "#,
        )
        .bind(&trip_ids)
        .bind(&masks)
        .fetch_all(&self.pool)
        .await?;

        let mut out: std::collections::HashMap<Uuid, Vec<FareAvailability>> =
            std::collections::HashMap::new();
        for row in rows {
            out.entry(row.try_get("trip_id")?)
                .or_default()
                .push(FareAvailability {
                    fare_class: row.try_get("fare_class")?,
                    available: row.try_get("available")?,
                    total: row.try_get("total")?,
                });
        }
        Ok(out)
    }

    /// Pool remainders for many (trip, span) pairs at once.
    async fn pool_availability_batch(
        &self,
        spans: &[(Uuid, i64)],
    ) -> Result<std::collections::HashMap<Uuid, Vec<PoolAvailability>>, StoreError> {
        // The mask's set bits ARE the segments, so from/to come back out
        // of it — keeping one shape for both batch queries.
        let mut trip_ids = Vec::with_capacity(spans.len());
        let mut froms = Vec::with_capacity(spans.len());
        let mut tos = Vec::with_capacity(spans.len());
        for (trip_id, mask) in spans {
            let mask = *mask as u64;
            trip_ids.push(*trip_id);
            froms.push(mask.trailing_zeros() as i32 + 1);
            tos.push((u64::BITS - mask.leading_zeros()) as i32);
        }

        let rows = sqlx::query(
            r#"
            SELECT q.trip_id, cu.code,
                   (SELECT min(x) FROM unnest(po.remaining[q.from_idx:q.to_idx]) AS x) AS remaining
            FROM unnest($1::uuid[], $2::int[], $3::int[]) AS q(trip_id, from_idx, to_idx)
            JOIN pool_occupancy po ON po.trip_id = q.trip_id
            JOIN capacity_units cu ON cu.id = po.unit_id
            ORDER BY q.trip_id, cu.code
            "#,
        )
        .bind(&trip_ids)
        .bind(&froms)
        .bind(&tos)
        .fetch_all(&self.pool)
        .await?;

        let mut out: std::collections::HashMap<Uuid, Vec<PoolAvailability>> =
            std::collections::HashMap::new();
        for row in rows {
            out.entry(row.try_get("trip_id")?)
                .or_default()
                .push(PoolAvailability {
                    code: row.try_get("code")?,
                    remaining: row.try_get::<Option<i32>, _>("remaining")?.unwrap_or(0),
                });
        }
        Ok(out)
    }

    /// Per-unit availability for one trip and an origin/destination pair on
    /// its route. `Ok(None)` if the trip does not exist.
    pub async fn trip_availability(
        &self,
        trip_id: Uuid,
        origin: &str,
        destination: &str,
    ) -> Result<Option<TripAvailability>, StoreError> {
        let Some(trip) = sqlx::query("SELECT route_id FROM trips WHERE id = $1")
            .bind(trip_id)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let route_id: Uuid = trip.try_get("route_id")?;

        // Availability is a read: a departed or cancelled trip still answers
        // (crew and support look at them). Selling is gated in
        // `resolve_target`.
        let (span, _) = self.resolve_span(route_id, origin, destination).await?;

        let seat_rows = sqlx::query(
            r#"
            SELECT cu.id AS unit_id, cu.code, cu.fare_class,
                   (so.occupied_mask & $2) = 0 AS available
            FROM seat_occupancy so
            JOIN capacity_units cu ON cu.id = so.unit_id
            WHERE so.trip_id = $1
            ORDER BY cu.code
            "#,
        )
        .bind(trip_id)
        .bind(span.mask() as i64)
        .fetch_all(&self.pool)
        .await?;

        let seats = seat_rows
            .into_iter()
            .map(|row| {
                Ok(SeatAvailability {
                    unit_id: row.try_get("unit_id")?,
                    code: row.try_get("code")?,
                    fare_class: row.try_get("fare_class")?,
                    available: row.try_get("available")?,
                    held: false,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(Some(TripAvailability {
            trip_id,
            origin: origin.to_string(),
            destination: destination.to_string(),
            from_index: span.from_index(),
            to_index: span.to_index(),
            seats,
            pools: self.pool_availability(trip_id, span).await?,
        }))
    }

    /// Map origin/destination codes to a segment span on `route_id`, plus
    /// the origin stop's departure offset in minutes from the trip's own
    /// departure — what turns a trip time into a boarding time for this
    /// particular leg.
    async fn resolve_span(
        &self,
        route_id: Uuid,
        origin: &str,
        destination: &str,
    ) -> Result<(SegmentSpan, i32), StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT l.code, rs.stop_index, rs.depart_offset_min
            FROM route_stops rs
            JOIN locations l ON l.id = rs.location_id
            WHERE rs.route_id = $1 AND l.code IN ($2, $3)
            ORDER BY rs.stop_index
            "#,
        )
        .bind(route_id)
        .bind(origin)
        .bind(destination)
        .fetch_all(&self.pool)
        .await?;

        let stops: Vec<Stop> = rows
            .iter()
            .map(|r| {
                Ok(Stop {
                    code: r.try_get("code")?,
                    index: r.try_get("stop_index")?,
                    depart_offset: r.try_get("depart_offset_min")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()?;
        pick_span(&stops, origin, destination)
    }

    async fn pool_availability(
        &self,
        trip_id: Uuid,
        span: SegmentSpan,
    ) -> Result<Vec<PoolAvailability>, StoreError> {
        // Postgres arrays are 1-based: span [from, to) is slice [from+1 : to].
        let rows = sqlx::query(
            r#"
            SELECT cu.code,
                   (SELECT min(x) FROM unnest(po.remaining[$2:$3]) AS x) AS remaining
            FROM pool_occupancy po
            JOIN capacity_units cu ON cu.id = po.unit_id
            WHERE po.trip_id = $1
            ORDER BY cu.code
            "#,
        )
        .bind(trip_id)
        .bind(i32::from(span.from_index()) + 1)
        .bind(i32::from(span.to_index()))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(PoolAvailability {
                    code: row.try_get("code")?,
                    remaining: row.try_get::<Option<i32>, _>("remaining")?.unwrap_or(0),
                })
            })
            .collect()
    }
}

/// One stop on a route, as `pick_span` needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stop {
    pub code: String,
    pub index: i16,
    pub depart_offset: i32,
}

/// Resolve `origin → destination` to a span over a route's stops.
///
/// A route may visit the same location twice — loops, out-and-back
/// services, anything that returns to base — so "the stop with this code"
/// is ambiguous. The rule is: the EARLIEST occurrence of the origin, then
/// the earliest occurrence of the destination *after* it.
///
/// Taking the first match of each (what this used to do) silently broke
/// the closing leg of any loop: on A→B→C→A, booking C→A matched A at
/// index 0, decided the journey ran backwards, and refused the sale.
pub(crate) fn pick_span(
    stops: &[Stop],
    origin: &str,
    destination: &str,
) -> Result<(SegmentSpan, i32), StoreError> {
    let mut ordered: Vec<&Stop> = stops.iter().collect();
    ordered.sort_by_key(|s| s.index);

    let from = ordered
        .iter()
        .find(|s| s.code == origin)
        .ok_or_else(|| StoreError::UnknownStop(origin.to_string()))?;

    // Distinguish "we do not call there at all" from "we call there, but
    // not after your origin" — different problems, different messages.
    if !ordered.iter().any(|s| s.code == destination) {
        return Err(StoreError::UnknownStop(destination.to_string()));
    }
    let to = ordered
        .iter()
        .find(|s| s.code == destination && s.index > from.index)
        .ok_or_else(|| StoreError::StopsOutOfOrder {
            origin: origin.to_string(),
            destination: destination.to_string(),
        })?;

    Ok((
        SegmentSpan::new(from.index as u8, to.index as u8)?,
        from.depart_offset,
    ))
}

// ---------------------------------------------------------------------------
// Executor-generic claim primitives. The same guarded statements work on a
// pool (standalone claims, Phase 2) or inside a transaction (order creation,
// Phase 3) — one SQL truth for both paths. All return rows affected:
// 1 = won, 0 = conflict or missing row.
// ---------------------------------------------------------------------------

pub async fn claim_seat_exec<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    trip_id: Uuid,
    unit_id: Uuid,
    span: SegmentSpan,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE seat_occupancy
        SET occupied_mask = occupied_mask | $3
        WHERE trip_id = $1 AND unit_id = $2 AND (occupied_mask & $3) = 0
        "#,
    )
    .bind(trip_id)
    .bind(unit_id)
    .bind(span.mask() as i64)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

pub async fn release_seat_exec<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    trip_id: Uuid,
    unit_id: Uuid,
    span: SegmentSpan,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE seat_occupancy
        SET occupied_mask = occupied_mask & ~($3::bigint)
        WHERE trip_id = $1 AND unit_id = $2 AND (occupied_mask & $3) = $3
        "#,
    )
    .bind(trip_id)
    .bind(unit_id)
    .bind(span.mask() as i64)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

pub async fn claim_pool_exec<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    trip_id: Uuid,
    unit_id: Uuid,
    span: SegmentSpan,
    qty: i32,
) -> Result<u64, sqlx::Error> {
    // Postgres arrays are 1-based: span [from, to) is slice [from+1 : to].
    let result = sqlx::query(
        r#"
        UPDATE pool_occupancy
        SET remaining = (
            SELECT array_agg(
                CASE WHEN idx BETWEEN $3 AND $4 THEN val - $5 ELSE val END
                ORDER BY idx)
            FROM unnest(remaining) WITH ORDINALITY AS t(val, idx)
        )
        WHERE trip_id = $1 AND unit_id = $2
          AND (SELECT min(val) FROM unnest(remaining[$3:$4]) AS val) >= $5
        "#,
    )
    .bind(trip_id)
    .bind(unit_id)
    .bind(i32::from(span.from_index()) + 1)
    .bind(i32::from(span.to_index()))
    .bind(qty)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// Guarded on not exceeding the unit's capacity, so releasing something
/// never claimed cannot inflate a pool.
pub async fn release_pool_exec<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    trip_id: Uuid,
    unit_id: Uuid,
    span: SegmentSpan,
    qty: i32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE pool_occupancy
        SET remaining = (
            SELECT array_agg(
                CASE WHEN idx BETWEEN $3 AND $4 THEN val + $5 ELSE val END
                ORDER BY idx)
            FROM unnest(remaining) WITH ORDINALITY AS t(val, idx)
        )
        WHERE trip_id = $1 AND unit_id = $2
          AND (SELECT max(val) FROM unnest(remaining[$3:$4]) AS val) + $5
              <= (SELECT pool_capacity FROM capacity_units WHERE id = $2)
        "#,
    )
    .bind(trip_id)
    .bind(unit_id)
    .bind(i32::from(span.from_index()) + 1)
    .bind(i32::from(span.to_index()))
    .bind(qty)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stops(spec: &[(&str, i16, i32)]) -> Vec<Stop> {
        spec.iter()
            .map(|(code, index, depart_offset)| Stop {
                code: (*code).to_string(),
                index: *index,
                depart_offset: *depart_offset,
            })
            .collect()
    }

    /// The linear case: a plain A→B→C→D service.
    #[test]
    fn linear_route_spans_and_offsets() {
        let route = stops(&[
            ("BTG", 0, 0),
            ("CTC", 1, 270),
            ("ILO", 2, 540),
            ("CEB", 3, 780),
        ]);

        let (span, offset) = pick_span(&route, "BTG", "CEB").unwrap();
        assert_eq!((span.from_index(), span.to_index()), (0, 3));
        assert_eq!(offset, 0, "offset is the ORIGIN's departure");

        let (span, offset) = pick_span(&route, "CTC", "ILO").unwrap();
        assert_eq!((span.from_index(), span.to_index()), (1, 2));
        assert_eq!(offset, 270);

        assert!(matches!(
            pick_span(&route, "CEB", "BTG"),
            Err(StoreError::StopsOutOfOrder { .. })
        ));
    }

    /// The regression: a service that returns to where it started.
    /// Booking the closing leg used to be refused outright, because the
    /// destination matched its FIRST occurrence — index 0, behind the
    /// origin — and looked like a backwards journey.
    #[test]
    fn loop_route_resolves_the_closing_leg() {
        let route = stops(&[
            ("AAA", 0, 0),
            ("BBB", 1, 60),
            ("CCC", 2, 150),
            ("AAA", 3, 240),
        ]);

        let (span, offset) = pick_span(&route, "CCC", "AAA").expect("the closing leg is sellable");
        assert_eq!(
            (span.from_index(), span.to_index()),
            (2, 3),
            "CCC→AAA is the last segment, not a reversed whole-route span"
        );
        assert_eq!(offset, 150, "boarding time comes from CCC, not from AAA");

        // And the outbound direction still takes the earliest origin.
        let (span, offset) = pick_span(&route, "AAA", "CCC").unwrap();
        assert_eq!((span.from_index(), span.to_index()), (0, 2));
        assert_eq!(offset, 0);
    }

    /// Origin repeated: take the earliest, so the span covers the whole
    /// journey the passenger is actually making.
    #[test]
    fn repeated_origin_uses_the_earliest_occurrence() {
        let route = stops(&[
            ("AAA", 0, 0),
            ("BBB", 1, 60),
            ("AAA", 2, 120),
            ("CCC", 3, 180),
        ]);
        let (span, _) = pick_span(&route, "AAA", "CCC").unwrap();
        assert_eq!((span.from_index(), span.to_index()), (0, 3));
    }

    /// A stop we never call at is a different error from one we call at
    /// in the wrong order — the caller can act on the second, not the first.
    #[test]
    fn unknown_and_out_of_order_stops_are_distinguished() {
        let route = stops(&[("AAA", 0, 0), ("BBB", 1, 60)]);
        assert!(matches!(
            pick_span(&route, "ZZZ", "BBB"),
            Err(StoreError::UnknownStop(code)) if code == "ZZZ"
        ));
        assert!(matches!(
            pick_span(&route, "AAA", "ZZZ"),
            Err(StoreError::UnknownStop(code)) if code == "ZZZ"
        ));
        assert!(matches!(
            pick_span(&route, "BBB", "AAA"),
            Err(StoreError::StopsOutOfOrder { .. })
        ));
        assert!(matches!(
            pick_span(&route, "AAA", "AAA"),
            Err(StoreError::StopsOutOfOrder { .. },),
        ));
    }
}
