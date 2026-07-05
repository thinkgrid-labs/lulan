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
    #[error("invalid segment span: {0}")]
    Span(#[from] SpanError),
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
    pub code: String,
    pub fare_class: String,
    pub available: bool,
}

/// A search hit: one trip serving the requested origin → destination.
#[derive(Debug, Serialize)]
pub struct TripSummary {
    pub trip_id: Uuid,
    pub route_code: String,
    pub departs_at: DateTime<Utc>,
    pub origin: String,
    pub destination: String,
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
    pub async fn resolve_target(
        &self,
        trip_id: Uuid,
        unit_code: &str,
        origin: &str,
        destination: &str,
    ) -> Result<Option<ClaimTarget>, StoreError> {
        let Some(row) = sqlx::query(
            r#"
            SELECT cu.id AS unit_id, cu.kind, cu.fare_class, t.route_id
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

        let route_id: Uuid = row.try_get("route_id")?;
        let span = self.resolve_span(route_id, origin, destination).await?;
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
    ) -> Result<Vec<TripSummary>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT t.id, r.code AS route_code, t.departs_at,
                   o.stop_index AS from_index, d.stop_index AS to_index
            FROM trips t
            JOIN routes r       ON r.id = t.route_id
            JOIN route_stops o  ON o.route_id = r.id
            JOIN locations lo   ON lo.id = o.location_id AND lo.code = $1
            JOIN route_stops d  ON d.route_id = r.id
            JOIN locations ld   ON ld.id = d.location_id AND ld.code = $2
            WHERE o.stop_index < d.stop_index
              AND t.service_date = $3
            ORDER BY t.departs_at
            "#,
        )
        .bind(origin)
        .bind(destination)
        .bind(date)
        .fetch_all(&self.pool)
        .await?;

        let mut trips = Vec::with_capacity(rows.len());
        for row in rows {
            let trip_id: Uuid = row.try_get("id")?;
            let from_index: i16 = row.try_get("from_index")?;
            let to_index: i16 = row.try_get("to_index")?;
            let span = SegmentSpan::new(from_index as u8, to_index as u8)?;

            trips.push(TripSummary {
                trip_id,
                route_code: row.try_get("route_code")?,
                departs_at: row.try_get("departs_at")?,
                origin: origin.to_string(),
                destination: destination.to_string(),
                from_index: span.from_index(),
                to_index: span.to_index(),
                seats: self.fare_availability(trip_id, span).await?,
                pools: self.pool_availability(trip_id, span).await?,
            });
        }
        Ok(trips)
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

        let span = self.resolve_span(route_id, origin, destination).await?;

        let seat_rows = sqlx::query(
            r#"
            SELECT cu.code, cu.fare_class, (so.occupied_mask & $2) = 0 AS available
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
                    code: row.try_get("code")?,
                    fare_class: row.try_get("fare_class")?,
                    available: row.try_get("available")?,
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

    /// Map origin/destination codes to a segment span on `route_id`.
    async fn resolve_span(
        &self,
        route_id: Uuid,
        origin: &str,
        destination: &str,
    ) -> Result<SegmentSpan, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT l.code, rs.stop_index
            FROM route_stops rs
            JOIN locations l ON l.id = rs.location_id
            WHERE rs.route_id = $1 AND l.code IN ($2, $3)
            "#,
        )
        .bind(route_id)
        .bind(origin)
        .bind(destination)
        .fetch_all(&self.pool)
        .await?;

        let index_of = |code: &str| -> Result<i16, StoreError> {
            rows.iter()
                .find(|r| r.get::<String, _>("code") == code)
                .map(|r| r.get::<i16, _>("stop_index"))
                .ok_or_else(|| StoreError::UnknownStop(code.to_string()))
        };
        let from = index_of(origin)?;
        let to = index_of(destination)?;
        if from >= to {
            return Err(StoreError::StopsOutOfOrder {
                origin: origin.to_string(),
                destination: destination.to_string(),
            });
        }
        Ok(SegmentSpan::new(from as u8, to as u8)?)
    }

    async fn fare_availability(
        &self,
        trip_id: Uuid,
        span: SegmentSpan,
    ) -> Result<Vec<FareAvailability>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT cu.fare_class,
                   count(*) FILTER (WHERE (so.occupied_mask & $2) = 0) AS available,
                   count(*) AS total
            FROM seat_occupancy so
            JOIN capacity_units cu ON cu.id = so.unit_id
            WHERE so.trip_id = $1
            GROUP BY cu.fare_class
            ORDER BY cu.fare_class
            "#,
        )
        .bind(trip_id)
        .bind(span.mask() as i64)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(FareAvailability {
                    fare_class: row.try_get("fare_class")?,
                    available: row.try_get("available")?,
                    total: row.try_get("total")?,
                })
            })
            .collect()
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
