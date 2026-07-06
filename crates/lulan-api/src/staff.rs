//! Staff RBAC (Phase 7.5): operator humans are IdP identities with a
//! `staff` row granting a role — never accounts in core. A JWT without a
//! staff row is just a customer; enrolment is an explicit, audited admin
//! action. Three roles, extractor-enforced:
//!
//! - `admin`   — everything (staff, keys, webhooks, plus ops+support)
//! - `ops`     — network, schedules, vessels, fares
//! - `support` — order search, refunds, manifests
//!
//! Bootstrap: `LULAN_BOOTSTRAP_ADMIN_STAFF="issuer|subject"` upserts the
//! first admin at boot, mirroring the API-key bootstrap.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaffRole {
    Admin,
    Ops,
    Support,
}

impl StaffRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            StaffRole::Admin => "admin",
            StaffRole::Ops => "ops",
            StaffRole::Support => "support",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "admin" => StaffRole::Admin,
            "ops" => StaffRole::Ops,
            "support" => StaffRole::Support,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct StaffMember {
    pub staff_id: Uuid,
    pub display_name: String,
    pub role: StaffRole,
}

/// Resolve the bearer JWT to an enrolled, active staff member.
pub async fn resolve(state: &AppState, parts: &Parts) -> Result<Option<StaffMember>, ApiError> {
    let Some(subject) = crate::identity::bearer_subject(state, &parts.headers) else {
        return Ok(None);
    };
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let row = sqlx::query(
        "SELECT id, display_name, role FROM staff
         WHERE issuer = $1 AND subject = $2 AND active",
    )
    .bind(&subject.issuer)
    .bind(&subject.subject)
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(row.map(|r| StaffMember {
        staff_id: r.get("id"),
        display_name: r.get("display_name"),
        role: StaffRole::parse(r.get::<String, _>("role").as_str())
            .expect("staff.role CHECK guarantees a known value"),
    }))
}

async fn require(
    parts: &mut Parts,
    state: &AppState,
    allowed: &[StaffRole],
    label: &'static str,
) -> Result<StaffMember, ApiError> {
    // No or invalid token = 401; a valid identity that is simply not
    // enrolled (or lacks the role) = 403.
    if crate::identity::bearer_subject(state, &parts.headers).is_none() {
        return Err(ApiError::Unauthorized("staff bearer token required"));
    }
    let member = resolve(state, parts)
        .await?
        .ok_or(ApiError::Forbidden("not enrolled as staff"))?;
    if member.role == StaffRole::Admin || allowed.contains(&member.role) {
        Ok(member)
    } else {
        Err(ApiError::Forbidden(label))
    }
}

/// Any enrolled staff (admin implies everything).
pub struct AnyStaff(pub StaffMember);
/// Network/schedule/fare management.
pub struct OpsStaff(pub StaffMember);
/// Order operations: search, refunds, manifests.
pub struct SupportStaff(pub StaffMember);
/// Staff/keys/webhooks management.
pub struct AdminStaff(pub StaffMember);

impl FromRequestParts<AppState> for AnyStaff {
    type Rejection = ApiError;
    async fn from_request_parts(p: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        require(
            p,
            s,
            &[StaffRole::Ops, StaffRole::Support],
            "staff role required",
        )
        .await
        .map(AnyStaff)
    }
}

impl FromRequestParts<AppState> for OpsStaff {
    type Rejection = ApiError;
    async fn from_request_parts(p: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        require(p, s, &[StaffRole::Ops], "ops role required")
            .await
            .map(OpsStaff)
    }
}

impl FromRequestParts<AppState> for SupportStaff {
    type Rejection = ApiError;
    async fn from_request_parts(p: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        require(p, s, &[StaffRole::Support], "support role required")
            .await
            .map(SupportStaff)
    }
}

impl FromRequestParts<AppState> for AdminStaff {
    type Rejection = ApiError;
    async fn from_request_parts(p: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        require(p, s, &[], "admin role required")
            .await
            .map(AdminStaff)
    }
}

/// Upsert the bootstrap admin staffer from `issuer|subject` (boot path).
pub async fn bootstrap_admin_staff(pool: &PgPool, spec: &str) -> anyhow::Result<()> {
    let Some((issuer, subject)) = spec.split_once('|') else {
        anyhow::bail!("LULAN_BOOTSTRAP_ADMIN_STAFF must be \"issuer|subject\"");
    };
    sqlx::query(
        "INSERT INTO staff (id, issuer, subject, display_name, role)
         VALUES ($1, $2, $3, 'bootstrap admin', 'admin')
         ON CONFLICT (issuer, subject) DO UPDATE SET active = true, role = 'admin'",
    )
    .bind(Uuid::new_v4())
    .bind(issuer)
    .bind(subject)
    .execute(pool)
    .await?;
    Ok(())
}
