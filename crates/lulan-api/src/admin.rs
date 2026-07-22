//! Admin operations API (Phase 7.5): `/v1/admin/*` — the backend the
//! operator's admin app drives. Role-gated at the extractor layer
//! (`admin` / `ops` / `support`; admin implies all), every mutation
//! audited with the human's staff_id.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lulan_engine::orders::{OrderStore, TransitionOutcome};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::auth::{Actor, audit};
use crate::error::ApiError;
use crate::staff::{OpsStaff, StaffRole, SupportStaff};
use crate::state::AppState;

fn db(state: &AppState) -> Result<&PgPool, ApiError> {
    state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))
}

/// How many orders a cancellation request settles before handing the
/// rest to the background drain. Small enough to keep the request
/// snappy, large enough that a typical trip finishes inline.
const INLINE_CASCADE_LIMIT: i64 = 25;

fn actor(staff_id: Uuid) -> Actor {
    Actor {
        api_key_id: None,
        staff_id: Some(staff_id),
    }
}

// ====================================================================
// Ticket signing keys (admin)
// ====================================================================

#[derive(Serialize)]
pub struct RotatedKey {
    kid: String,
    public_key: String,
}

/// POST /v1/admin/ticket-keys/rotate — mint a new signing key.
///
/// Takes effect immediately on every replica: issuance reads the active
/// key per ticket rather than caching one at boot.
///
/// Previous keys stay published by `GET /v1/ticket-keys`, because tickets
/// already in passengers' wallets were signed with them and must keep
/// verifying until they expire. Rotation changes what gets signed next —
/// it does not invalidate what was signed before. Responding to a leaked
/// key means rotating AND revoking the tickets it signed.
pub async fn rotate_ticket_key(
    State(state): State<AppState>,
    admin: AdminStaffOrKey,
) -> Result<(StatusCode, Json<RotatedKey>), ApiError> {
    let pool = db(&state)?;
    let signer = lulan_engine::ticket::TicketSigner::rotate(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        admin.actor,
        "ticket_key.rotated",
        json!({ "kid": signer.kid }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((
        StatusCode::CREATED,
        Json(RotatedKey {
            public_key: signer.public_key_b64(),
            kid: signer.kid,
        }),
    ))
}

// ====================================================================
// Staff enrolment (admin)
// ====================================================================

#[derive(Deserialize)]
pub struct EnrollStaffRequest {
    /// Defaults to the configured IdP issuer when omitted.
    #[serde(default)]
    issuer: Option<String>,
    subject: String,
    display_name: String,
    #[serde(default)]
    email: Option<String>,
    role: String,
}

#[derive(Serialize)]
pub struct StaffRecord {
    id: Uuid,
    issuer: String,
    subject: String,
    display_name: String,
    email: Option<String>,
    role: StaffRole,
    active: bool,
}

/// POST /v1/admin/staff — enrol an IdP identity as staff.
pub async fn enroll_staff(
    State(state): State<AppState>,
    admin: AdminStaffOrKey,
    Json(req): Json<EnrollStaffRequest>,
) -> Result<(StatusCode, Json<StaffRecord>), ApiError> {
    let pool = db(&state)?;
    let role = StaffRole::parse(&req.role)
        .ok_or_else(|| ApiError::BadRequest("role must be admin, ops, or support".into()))?;
    let issuer = req
        .issuer
        .or_else(|| std::env::var("LULAN_IDP_ISSUER").ok())
        .ok_or_else(|| ApiError::BadRequest("issuer required (no IdP configured)".into()))?;
    if req.subject.trim().is_empty() || req.display_name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "subject and display_name are required".into(),
        ));
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff (id, issuer, subject, email, display_name, role)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (issuer, subject)
             DO UPDATE SET role = excluded.role, display_name = excluded.display_name,
                           email = excluded.email, active = true
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&issuer)
    .bind(req.subject.trim())
    .bind(&req.email)
    .bind(req.display_name.trim())
    .bind(role.as_str())
    .fetch_one(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    audit(
        pool,
        admin.actor,
        "staff.enrolled",
        json!({ "id": id, "subject": req.subject.trim(), "role": role.as_str() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok((
        StatusCode::CREATED,
        Json(StaffRecord {
            id,
            issuer,
            subject: req.subject.trim().to_string(),
            display_name: req.display_name.trim().to_string(),
            email: req.email,
            role,
            active: true,
        }),
    ))
}

/// GET /v1/admin/staff (admin)
pub async fn list_staff(
    State(state): State<AppState>,
    _admin: AdminStaffOrKey,
) -> Result<Json<Vec<StaffRecord>>, ApiError> {
    let pool = db(&state)?;
    let rows = sqlx::query(
        "SELECT id, issuer, subject, email, display_name, role, active
         FROM staff ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(
        rows.into_iter()
            .map(|r| StaffRecord {
                id: r.get("id"),
                issuer: r.get("issuer"),
                subject: r.get("subject"),
                email: r.get("email"),
                display_name: r.get("display_name"),
                role: StaffRole::parse(r.get::<String, _>("role").as_str()).expect("checked"),
                active: r.get("active"),
            })
            .collect(),
    ))
}

/// DELETE /v1/admin/staff/{id} (admin) — revoke access.
pub async fn revoke_staff(
    State(state): State<AppState>,
    admin: AdminStaffOrKey,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    let updated = sqlx::query("UPDATE staff SET active = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("staff {id} not found")));
    }
    audit(pool, admin.actor, "staff.revoked", json!({ "id": id }))
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(json!({ "revoked": id })))
}

/// Admin surface accepts an admin staff JWT OR an operator_admin API key
/// (so the bootstrap key can enrol the first humans).
pub struct AdminStaffOrKey {
    pub actor: Actor,
}

impl axum::extract::FromRequestParts<AppState> for AdminStaffOrKey {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let crate::auth::AdminAuth(actor) =
            crate::auth::AdminAuth::from_request_parts(parts, state).await?;
        Ok(AdminStaffOrKey { actor })
    }
}

// ====================================================================
// Fare-rule publishing (ops)
// ====================================================================

/// GET /v1/admin/fare-rules — active + recent rulesets.
pub async fn list_fare_rules(
    State(state): State<AppState>,
    _ops: OpsStaff,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    // Active first, then newest. Ordering by date alone hid the live
    // ruleset once an operator had published more than a page of them —
    // the one row this list exists to show.
    let rows = sqlx::query(
        "SELECT id, active, created_at FROM fare_rules
         ORDER BY active DESC, created_at DESC LIMIT 20",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let list: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "active": r.get::<bool, _>("active"),
                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "rulesets": list })))
}

/// POST /v1/admin/fare-rules (ops) — validate + publish + activate a new
/// ruleset. The previous one stays for rollback.
pub async fn publish_fare_rules(
    State(state): State<AppState>,
    ops: OpsStaff,
    Json(rules): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = db(&state)?;
    // Validate against the pricing schema AND smoke-evaluate so a ruleset
    // that parses but cannot price anything is rejected loudly.
    let parsed: lulan_pricing::rules::FareRuleSet = serde_json::from_value(rules.clone())
        .map_err(|e| ApiError::BadRequest(format!("ruleset does not match the schema: {e}")))?;
    if parsed.base_fare_per_segment.is_empty() {
        return Err(ApiError::BadRequest(
            "ruleset has no base fares — it could sell nothing".into(),
        ));
    }

    let id = Uuid::new_v4();
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    sqlx::query("UPDATE fare_rules SET active = false WHERE active")
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    sqlx::query("INSERT INTO fare_rules (id, active, rules) VALUES ($1, true, $2)")
        .bind(id)
        .bind(&rules)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    audit(
        pool,
        actor(ops.0.staff_id),
        "fare_rules.published",
        json!({ "id": id }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "active": true })),
    ))
}

/// POST /v1/admin/fare-rules/{id}/activate (ops) — rollback/switch.
pub async fn activate_fare_rules(
    State(state): State<AppState>,
    ops: OpsStaff,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM fare_rules WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    if exists.is_none() {
        return Err(ApiError::NotFound(format!("ruleset {id} not found")));
    }
    sqlx::query("UPDATE fare_rules SET active = false WHERE active")
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    sqlx::query("UPDATE fare_rules SET active = true WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        actor(ops.0.staff_id),
        "fare_rules.activated",
        json!({ "id": id }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(json!({ "id": id, "active": true })))
}

// ====================================================================
// Network CRUD (ops): locations, routes, vessels, trips
// ====================================================================

#[derive(Deserialize)]
pub struct CreateLocationRequest {
    code: String,
    name: String,
    #[serde(default = "default_tz")]
    timezone: String,
}

fn default_tz() -> String {
    "UTC".into()
}

/// POST /v1/admin/locations (ops)
pub async fn create_location(
    State(state): State<AppState>,
    ops: OpsStaff,
    Json(req): Json<CreateLocationRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = db(&state)?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO locations (id, code, name, timezone) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(req.code.trim())
        .bind(req.name.trim())
        .bind(&req.timezone)
        .execute(pool)
        .await
        .map_err(unique_conflict("location code"))?;
    audit(
        pool,
        actor(ops.0.staff_id),
        "location.created",
        json!({ "id": id, "code": req.code.trim() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
pub struct RouteStopRequest {
    location_code: String,
    #[serde(default)]
    arrive_offset_min: i32,
    #[serde(default)]
    depart_offset_min: i32,
}

#[derive(Deserialize)]
pub struct CreateRouteRequest {
    code: String,
    name: String,
    /// Ordered stops with schedule offsets (minutes from origin departure).
    stops: Vec<RouteStopRequest>,
}

/// POST /v1/admin/routes (ops)
pub async fn create_route(
    State(state): State<AppState>,
    ops: OpsStaff,
    Json(req): Json<CreateRouteRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = db(&state)?;
    if req.stops.len() < 2 {
        return Err(ApiError::BadRequest(
            "a route needs at least 2 stops".into(),
        ));
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO routes (id, code, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(req.code.trim())
        .bind(req.name.trim())
        .execute(&mut *tx)
        .await
        .map_err(unique_conflict("route code"))?;
    for (index, stop) in req.stops.iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO route_stops (route_id, stop_index, location_id, arrive_offset_min, depart_offset_min)
             SELECT $1, $2, id, $4, $5 FROM locations WHERE code = $3",
        )
        .bind(id)
        .bind(index as i16)
        .bind(stop.location_code.trim())
        .bind(stop.arrive_offset_min)
        .bind(stop.depart_offset_min)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
        if inserted == 0 {
            return Err(ApiError::BadRequest(format!(
                "unknown location {:?}",
                stop.location_code
            )));
        }
    }
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        actor(ops.0.staff_id),
        "route.created",
        json!({ "id": id, "code": req.code.trim(), "stops": req.stops.len() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
pub struct SeatSpec {
    code: String,
    fare_class: String,
}

#[derive(Deserialize)]
pub struct PoolSpec {
    code: String,
    capacity: i32,
}

#[derive(Deserialize)]
pub struct CreateVesselRequest {
    code: String,
    name: String,
    kind: String,
    #[serde(default)]
    seats: Vec<SeatSpec>,
    #[serde(default)]
    pools: Vec<PoolSpec>,
}

/// POST /v1/admin/vessels (ops) — a capacity template (vehicle).
pub async fn create_vessel(
    State(state): State<AppState>,
    ops: OpsStaff,
    Json(req): Json<CreateVesselRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = db(&state)?;
    if req.seats.is_empty() && req.pools.is_empty() {
        return Err(ApiError::BadRequest(
            "a vehicle needs seats or pools".into(),
        ));
    }
    if !["bus", "ferry", "aircraft", "other"].contains(&req.kind.as_str()) {
        return Err(ApiError::BadRequest(
            "kind must be bus, ferry, aircraft, or other".into(),
        ));
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO resources (id, code, name, kind) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(req.code.trim())
        .bind(req.name.trim())
        .bind(&req.kind)
        .execute(&mut *tx)
        .await
        .map_err(unique_conflict("vehicle code"))?;
    for seat in &req.seats {
        sqlx::query(
            "INSERT INTO capacity_units (id, resource_id, kind, code, fare_class)
             VALUES ($1, $2, 'seat', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(seat.code.trim())
        .bind(seat.fare_class.trim())
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    }
    for pool_spec in &req.pools {
        sqlx::query(
            "INSERT INTO capacity_units (id, resource_id, kind, code, pool_capacity)
             VALUES ($1, $2, 'pool', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(pool_spec.code.trim())
        .bind(pool_spec.capacity)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    }
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        actor(ops.0.staff_id),
        "vessel.created",
        json!({ "id": id, "code": req.code.trim(), "seats": req.seats.len(), "pools": req.pools.len() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
pub struct CreateTripsRequest {
    route_code: String,
    vessel_code: String,
    #[serde(default)]
    operator_code: Option<String>,
    #[serde(default)]
    service_number: Option<String>,
    /// One trip per departure timestamp (UTC).
    departures: Vec<DateTime<Utc>>,
}

/// POST /v1/admin/trips (ops) — schedule departures.
pub async fn create_trips(
    State(state): State<AppState>,
    ops: OpsStaff,
    Json(req): Json<CreateTripsRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let pool = db(&state)?;
    if req.departures.is_empty() {
        return Err(ApiError::BadRequest("departures is empty".into()));
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let route: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT r.id, count(rs.*) - 1 FROM routes r
         JOIN route_stops rs ON rs.route_id = r.id
         WHERE r.code = $1 GROUP BY r.id",
    )
    .bind(req.route_code.trim())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let (route_id, segments) =
        route.ok_or_else(|| ApiError::BadRequest(format!("unknown route {:?}", req.route_code)))?;
    let resource_id: Uuid = sqlx::query_scalar("SELECT id FROM resources WHERE code = $1")
        .bind(req.vessel_code.trim())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .ok_or_else(|| ApiError::BadRequest(format!("unknown vessel {:?}", req.vessel_code)))?;
    let operator_id: Option<Uuid> = match &req.operator_code {
        Some(code) => Some(
            sqlx::query_scalar("SELECT id FROM operators WHERE code = $1")
                .bind(code.trim())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?
                .ok_or_else(|| ApiError::BadRequest(format!("unknown operator {code:?}")))?,
        ),
        None => None,
    };
    let units: Vec<(Uuid, Option<i32>)> =
        sqlx::query_as("SELECT id, pool_capacity FROM capacity_units WHERE resource_id = $1")
            .bind(resource_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;

    let mut trip_ids = Vec::with_capacity(req.departures.len());
    for departs_at in &req.departures {
        let trip_id = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO trips (id, route_id, resource_id, operator_id, service_number, service_date, departs_at, segment_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (route_id, resource_id, departs_at) DO NOTHING",
        )
        .bind(trip_id)
        .bind(route_id)
        .bind(resource_id)
        .bind(operator_id)
        .bind(&req.service_number)
        .bind(departs_at.date_naive())
        .bind(departs_at)
        .bind(segments as i16)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
        if inserted == 0 {
            continue; // already scheduled
        }
        for (unit_id, pool_capacity) in &units {
            match pool_capacity {
                None => sqlx::query(
                    "INSERT INTO seat_occupancy (trip_id, unit_id, occupied_mask) VALUES ($1, $2, 0)",
                )
                .bind(trip_id)
                .bind(unit_id)
                .execute(&mut *tx)
                .await
                .map(|_| ())
                .map_err(|e| ApiError::Internal(e.into()))?,
                Some(capacity) => sqlx::query(
                    "INSERT INTO pool_occupancy (trip_id, unit_id, remaining)
                     VALUES ($1, $2, array_fill($3::int, ARRAY[$4::int]))",
                )
                .bind(trip_id)
                .bind(unit_id)
                .bind(capacity)
                .bind(segments as i32)
                .execute(&mut *tx)
                .await
                .map(|_| ())
                .map_err(|e| ApiError::Internal(e.into()))?,
            }
        }
        trip_ids.push(trip_id);
    }
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        actor(ops.0.staff_id),
        "trips.created",
        json!({ "route": req.route_code.trim(), "count": trip_ids.len() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok((StatusCode::CREATED, Json(json!({ "trip_ids": trip_ids }))))
}

/// POST /v1/admin/trips/{id}/cancel (ops) — cancel the departure and
/// cascade: unpaid orders are cancelled, paid/ticketed orders refunded
/// (provider refund + seats released + tickets voided). Webhooks fire via
/// POST /v1/admin/trips/{id}/cancel
///
/// Cancelling the trip is immediate and authoritative — it stops the
/// departure being sold the moment this returns. Settling the orders on it
/// is not done here beyond a first bounded batch: a full sailing can be
/// hundreds of orders, each needing a provider round-trip, which would turn
/// one request into a multi-minute transaction that fails halfway.
///
/// The rest drains in the background off `trips.status`, so the work is
/// durable and resumable rather than tied to this connection. `remaining`
/// says how much is still outstanding.
pub async fn cancel_trip(
    State(state): State<AppState>,
    ops: OpsStaff,
    Path(trip_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    let updated =
        sqlx::query("UPDATE trips SET status = 'cancelled' WHERE id = $1 AND status = 'scheduled'")
            .bind(trip_id)
            .execute(pool)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?
            .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!(
            "trip {trip_id} not found or already cancelled"
        )));
    }

    let stats = OrderStore::new(pool.clone())
        .settle_cancelled_trips(state.payments.as_ref(), INLINE_CASCADE_LIMIT)
        .await?;

    audit(
        pool,
        actor(ops.0.staff_id),
        "trip.cancelled",
        json!({ "trip_id": trip_id, "settled": stats }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(json!({
        "trip_id": trip_id,
        "orders_cancelled": stats.cancelled,
        "orders_refunded": stats.refunded,
        "failures": stats.failed,
        "remaining": stats.remaining,
    })))
}

// ====================================================================
// Order operations (support): search, refund, manifest
// ====================================================================

#[derive(Deserialize)]
pub struct OrderSearchParams {
    #[serde(default)]
    contact: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    trip_id: Option<Uuid>,
}

/// GET /v1/admin/orders (support) — find bookings by contact, passenger
/// name, or trip.
pub async fn search_orders(
    State(state): State<AppState>,
    _support: SupportStaff,
    Query(params): Query<OrderSearchParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    if params.contact.is_none() && params.name.is_none() && params.trip_id.is_none() {
        return Err(ApiError::BadRequest(
            "filter by contact, name, or trip_id".into(),
        ));
    }
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT o.id, o.passenger_name, o.status, o.total_minor, o.currency,
               o.guest_contact, o.created_at
        FROM orders o
        LEFT JOIN passengers p ON p.order_id = o.id
        LEFT JOIN order_items oi ON oi.order_id = o.id
        WHERE ($1::text IS NULL OR o.guest_contact ILIKE '%' || $1 || '%')
          AND ($2::text IS NULL OR p.full_name ILIKE '%' || $2 || '%')
          AND ($3::uuid IS NULL OR oi.trip_id = $3)
        ORDER BY o.created_at DESC
        LIMIT 50
        "#,
    )
    .bind(&params.contact)
    .bind(&params.name)
    .bind(params.trip_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let orders: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "order_id": r.get::<Uuid, _>("id"),
                "passenger_name": r.get::<String, _>("passenger_name"),
                "status": r.get::<String, _>("status"),
                "total_minor": r.get::<i64, _>("total_minor"),
                "currency": r.get::<String, _>("currency"),
                "guest_contact": r.get::<Option<String>, _>("guest_contact"),
                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "orders": orders })))
}

/// POST /v1/admin/orders/{id}/refund (support) — full refund through the
/// payment port; releases every leg's claims and voids tickets.
pub async fn refund_order(
    State(state): State<AppState>,
    support: SupportStaff,
    Path(order_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    let row = sqlx::query("SELECT payment_intent_id, total_minor FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .ok_or_else(|| ApiError::NotFound(format!("order {order_id} not found")))?;

    // Money back first (provider port); only then release inventory.
    let intent: Option<String> = row.get("payment_intent_id");
    let total: i64 = row.get("total_minor");
    if let Some(intent) = &intent {
        // Money first, inventory second: a failed refund must not free the
        // seat, so this propagates instead of being logged and skipped.
        state
            .payments
            .refund(intent, total)
            .await
            .map_err(crate::orders::payment_error)?;
    }

    let store = OrderStore::new(pool.clone());
    match store.refund(order_id).await? {
        TransitionOutcome::Applied(status) => {
            audit(
                pool,
                actor(support.0.staff_id),
                "order.refunded",
                json!({ "order_id": order_id, "amount_minor": total }),
            )
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
            Ok(Json(json!({ "order_id": order_id, "status": status })))
        }
        TransitionOutcome::NoOp(current) => Err(ApiError::Conflict(format!(
            "order cannot be refunded in state {:?}",
            current.as_str()
        ))),
        TransitionOutcome::NotFound => {
            Err(ApiError::NotFound(format!("order {order_id} not found")))
        }
    }
}

/// GET /v1/admin/trips/{id}/manifest (support) — who is on this
/// departure: passengers, seats, ticket + boarding state.
pub async fn trip_manifest(
    State(state): State<AppState>,
    _support: SupportStaff,
    Path(trip_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = db(&state)?;
    let rows = sqlx::query(
        r#"
        SELECT o.id AS order_id, o.status AS order_status,
               p.full_name, p.passenger_type,
               oi.unit_code, oi.from_index, oi.to_index,
               t.status AS ticket_status
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        LEFT JOIN passengers p ON p.id = oi.passenger_id
        LEFT JOIN tickets t ON t.order_id = o.id AND t.passenger_id = oi.passenger_id
                            AND t.trip_id = oi.trip_id AND t.unit_id = oi.unit_id
        WHERE oi.trip_id = $1 AND oi.kind = 'seat'
          AND o.status IN ('paid', 'ticketed', 'boarded')
        ORDER BY oi.unit_code
        "#,
    )
    .bind(trip_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let manifest: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "seat": r.get::<String, _>("unit_code"),
                "passenger": r.get::<Option<String>, _>("full_name"),
                "passenger_type": r.get::<Option<String>, _>("passenger_type"),
                "from_index": r.get::<i16, _>("from_index"),
                "to_index": r.get::<i16, _>("to_index"),
                "order_id": r.get::<Uuid, _>("order_id"),
                "order_status": r.get::<String, _>("order_status"),
                "ticket_status": r.get::<Option<String>, _>("ticket_status"),
            })
        })
        .collect();
    Ok(Json(json!({ "trip_id": trip_id, "seats": manifest })))
}

fn unique_conflict(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict(format!("{what} already exists"))
        }
        _ => ApiError::Internal(e.into()),
    }
}
