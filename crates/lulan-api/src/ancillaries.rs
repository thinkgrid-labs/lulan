//! Ancillaries: operator-defined add-ons (baggage, meals, insurance,
//! priority boarding) sold alongside the fare.
//!
//! Deliberately decoupled from holds: nothing here is scarce, so there is
//! nothing to hold — the catalog is public, and purchased add-ons ride the
//! quote → order flow as priced lines under the same HMAC token guarantee
//! as seats. Scarce "ancillaries" are capacity units instead (extra
//! legroom = a seat, baggage kilos = the CARGO_KG pool).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::auth::{AdminAuth, audit};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct Ancillary {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub price_minor: i64,
    /// `passenger` (one per passenger) or `order`.
    pub per: String,
    /// `journey` (tied to one leg) or `itinerary` (whole booking).
    pub scope: String,
}

fn row_to_ancillary(row: &sqlx::postgres::PgRow) -> Result<Ancillary, sqlx::Error> {
    Ok(Ancillary {
        id: row.try_get("id")?,
        code: row.try_get("code")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        kind: row.try_get("kind")?,
        price_minor: row.try_get("price_minor")?,
        per: row.try_get("per")?,
        scope: row.try_get("scope")?,
    })
}

/// Active catalog, keyed by code — what quote/order pricing validates
/// against.
pub async fn active_catalog(
    pool: &PgPool,
) -> Result<std::collections::HashMap<String, Ancillary>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, code, name, description, kind, price_minor, per, scope
         FROM ancillaries WHERE active ORDER BY kind, code",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| row_to_ancillary(row).map(|a| (a.code.clone(), a)))
        .collect()
}

/// GET /v1/ancillaries — the storefront's add-on catalog (public).
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<Ancillary>>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let rows = sqlx::query(
        "SELECT id, code, name, description, kind, price_minor, per, scope
         FROM ancillaries WHERE active ORDER BY kind, code",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let catalog = rows
        .iter()
        .map(row_to_ancillary)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(catalog))
}

#[derive(Deserialize)]
pub struct CreateAncillaryRequest {
    code: String,
    name: String,
    #[serde(default)]
    description: String,
    kind: String,
    price_minor: i64,
    #[serde(default = "default_per")]
    per: String,
    #[serde(default = "default_scope")]
    scope: String,
}

fn default_per() -> String {
    "passenger".into()
}

fn default_scope() -> String {
    "itinerary".into()
}

/// POST /v1/ancillaries (admin) — add a catalog entry.
pub async fn create(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<CreateAncillaryRequest>,
) -> Result<(StatusCode, Json<Ancillary>), ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    if req.code.trim().is_empty() || req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("code and name are required".into()));
    }
    if !["passenger", "order"].contains(&req.per.as_str()) {
        return Err(ApiError::BadRequest(
            "per must be passenger or order".into(),
        ));
    }
    if !["journey", "itinerary"].contains(&req.scope.as_str()) {
        return Err(ApiError::BadRequest(
            "scope must be journey or itinerary".into(),
        ));
    }
    if req.price_minor < 0 {
        return Err(ApiError::BadRequest("price_minor must be ≥ 0".into()));
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ancillaries (id, code, name, description, kind, price_minor, per, scope)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(req.code.trim())
    .bind(req.name.trim())
    .bind(&req.description)
    .bind(&req.kind)
    .bind(req.price_minor)
    .bind(&req.per)
    .bind(&req.scope)
    .execute(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => ApiError::Conflict(format!(
            "ancillary code {:?} already exists (deactivated entries keep their code)",
            req.code.trim()
        )),
        _ => ApiError::Internal(e.into()),
    })?;
    audit(
        pool,
        admin.0.key_id,
        "ancillary.created",
        serde_json::json!({ "id": id, "code": req.code.trim(), "price_minor": req.price_minor }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok((
        StatusCode::CREATED,
        Json(Ancillary {
            id,
            code: req.code.trim().to_string(),
            name: req.name.trim().to_string(),
            description: req.description,
            kind: req.kind,
            price_minor: req.price_minor,
            per: req.per,
            scope: req.scope,
        }),
    ))
}

/// DELETE /v1/ancillaries/{id} (admin) — deactivate (sold orders keep
/// their snapshots).
pub async fn remove(
    State(state): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let updated = sqlx::query("UPDATE ancillaries SET active = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("ancillary {id} not found")));
    }
    audit(
        pool,
        admin.0.key_id,
        "ancillary.deactivated",
        serde_json::json!({ "id": id }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(serde_json::json!({ "deactivated": id })))
}

// ---- Quote/order pricing --------------------------------------------

/// An add-on line as clients request it (same shape at quote and order —
/// that's what makes the token match exact).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AncillaryLine {
    pub code: String,
    /// Required for journey-scoped ancillaries: which leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trip_id: Option<Uuid>,
    /// Required for per-passenger ancillaries: index into passengers[].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passenger: Option<usize>,
    #[serde(default = "default_quantity")]
    pub quantity: i32,
}

fn default_quantity() -> i32 {
    1
}

/// A priced line, catalog-validated.
#[derive(Debug, Clone)]
pub struct PricedAncillary {
    pub ancillary: Ancillary,
    pub line: AncillaryLine,
    pub total_minor: i64,
}

/// Validate lines against the active catalog and the itinerary's trips,
/// and price them (flat catalog price × quantity).
pub async fn price_lines(
    pool: &PgPool,
    lines: &[AncillaryLine],
    itinerary_trips: &[Uuid],
) -> Result<Vec<PricedAncillary>, ApiError> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = active_catalog(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let mut priced = Vec::with_capacity(lines.len());
    for line in lines {
        let ancillary = catalog.get(&line.code).ok_or_else(|| {
            ApiError::BadRequest(format!("unknown or inactive ancillary {:?}", line.code))
        })?;
        if line.quantity <= 0 {
            return Err(ApiError::BadRequest(format!(
                "quantity must be positive for {}",
                line.code
            )));
        }
        match ancillary.scope.as_str() {
            "journey" => {
                let Some(trip_id) = line.trip_id else {
                    return Err(ApiError::BadRequest(format!(
                        "{} is journey-scoped: pass the leg's trip_id",
                        line.code
                    )));
                };
                if !itinerary_trips.contains(&trip_id) {
                    return Err(ApiError::BadRequest(format!(
                        "{}: trip {trip_id} is not part of this itinerary",
                        line.code
                    )));
                }
            }
            _ => {
                if line.trip_id.is_some() {
                    return Err(ApiError::BadRequest(format!(
                        "{} covers the whole itinerary: omit trip_id",
                        line.code
                    )));
                }
            }
        }
        if ancillary.per == "passenger" && line.passenger.is_none() {
            return Err(ApiError::BadRequest(format!(
                "{} is per-passenger: pass the passenger index",
                line.code
            )));
        }
        if ancillary.per == "order" && line.passenger.is_some() {
            return Err(ApiError::BadRequest(format!(
                "{} is order-level: omit passenger",
                line.code
            )));
        }

        priced.push(PricedAncillary {
            total_minor: ancillary.price_minor * line.quantity as i64,
            ancillary: ancillary.clone(),
            line: line.clone(),
        });
    }
    Ok(priced)
}
