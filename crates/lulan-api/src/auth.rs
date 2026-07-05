//! API-key authentication (Phase 6). Server-to-server credentials with
//! three roles, enforced at the extractor layer:
//!
//! - `operator_admin` — manage webhooks and API keys (audited)
//! - `integration`    — trusted storefront backends (read any order)
//! - `validator`      — ticket-validation devices — gates, crew handhelds (sync scans)
//!
//! Keys look like `llk_<64 hex chars>`; only their SHA-256 is stored.
//! The first admin key comes from the `LULAN_BOOTSTRAP_ADMIN_KEY` env var
//! (hashed and upserted at boot) — after that, mint keys via the API.

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiRole {
    OperatorAdmin,
    Integration,
    Validator,
}

impl ApiRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiRole::OperatorAdmin => "operator_admin",
            ApiRole::Integration => "integration",
            ApiRole::Validator => "validator",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "operator_admin" => ApiRole::OperatorAdmin,
            "integration" => ApiRole::Integration,
            "validator" => ApiRole::Validator,
            _ => return None,
        })
    }
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

fn generate_key() -> String {
    format!("llk_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// The authenticated API key, resolved from `X-Api-Key` or
/// `Authorization: Bearer llk_…`.
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    pub key_id: Uuid,
    pub role: ApiRole,
}

fn bearer_or_api_key(parts: &Parts) -> Option<String> {
    if let Some(value) = parts.headers.get("x-api-key") {
        return value.to_str().ok().map(str::to_string);
    }
    let auth = parts.headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    token.starts_with("llk_").then(|| token.to_string())
}

pub async fn authenticate(pool: &PgPool, key: &str) -> Result<Option<ApiKeyAuth>, sqlx::Error> {
    let Some(row) = sqlx::query("SELECT id, role FROM api_keys WHERE key_hash = $1 AND active")
        .bind(hash_key(key))
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(ApiKeyAuth {
        key_id: row.try_get("id")?,
        role: ApiRole::parse(row.get::<String, _>("role").as_str())
            .expect("api_keys.role CHECK guarantees a known value"),
    }))
}

impl FromRequestParts<AppState> for ApiKeyAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let pool = state
            .db
            .as_ref()
            .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
        let key = bearer_or_api_key(parts)
            .ok_or(ApiError::Unauthorized("API key required (X-Api-Key)"))?;
        authenticate(pool, &key)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?
            .ok_or(ApiError::Unauthorized("unknown or inactive API key"))
    }
}

/// Extractor requiring the `operator_admin` role.
pub struct AdminAuth(pub ApiKeyAuth);

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = ApiKeyAuth::from_request_parts(parts, state).await?;
        if auth.role != ApiRole::OperatorAdmin {
            return Err(ApiError::Forbidden("operator_admin role required"));
        }
        Ok(AdminAuth(auth))
    }
}

/// Extractor for boarding devices: `validator` or `operator_admin`.
pub struct DeviceAuth(pub ApiKeyAuth);

impl FromRequestParts<AppState> for DeviceAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = ApiKeyAuth::from_request_parts(parts, state).await?;
        if !matches!(auth.role, ApiRole::Validator | ApiRole::OperatorAdmin) {
            return Err(ApiError::Forbidden("validator role required"));
        }
        Ok(DeviceAuth(auth))
    }
}

/// Record an audited admin action.
pub async fn audit(
    pool: &PgPool,
    key_id: Uuid,
    action: &str,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_log (api_key_id, action, detail) VALUES ($1, $2, $3)")
        .bind(key_id)
        .bind(action)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

/// Upsert the bootstrap admin key from the environment (boot path).
pub async fn bootstrap_admin_key(pool: &PgPool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO api_keys (id, key_hash, label, role)
         VALUES ($1, $2, 'bootstrap', 'operator_admin')
         ON CONFLICT (key_hash) DO UPDATE SET active = true",
    )
    .bind(Uuid::new_v4())
    .bind(hash_key(key))
    .execute(pool)
    .await?;
    Ok(())
}

// ---- Admin endpoints -------------------------------------------------

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    label: String,
    role: String,
}

#[derive(Serialize)]
pub struct CreatedKey {
    id: Uuid,
    label: String,
    role: ApiRole,
    /// Shown exactly once — only the hash is stored.
    key: String,
}

/// POST /v1/api-keys (admin) — mint a key; the plaintext is returned once.
pub async fn create_key(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<CreateKeyRequest>,
) -> Result<Json<CreatedKey>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let role = ApiRole::parse(&req.role).ok_or_else(|| {
        ApiError::BadRequest("role must be operator_admin, integration, or validator".into())
    })?;
    if req.label.trim().is_empty() {
        return Err(ApiError::BadRequest("label is required".into()));
    }

    let key = generate_key();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys (id, key_hash, label, role) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(hash_key(&key))
        .bind(req.label.trim())
        .bind(role.as_str())
        .execute(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    audit(
        pool,
        admin.0.key_id,
        "api_key.created",
        serde_json::json!({ "id": id, "label": req.label.trim(), "role": role.as_str() }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok(Json(CreatedKey {
        id,
        label: req.label.trim().to_string(),
        role,
        key,
    }))
}

/// DELETE /v1/api-keys/{id} (admin) — deactivate.
pub async fn revoke_key(
    State(state): State<AppState>,
    admin: AdminAuth,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let updated = sqlx::query("UPDATE api_keys SET active = false WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::NotFound(format!("api key {id} not found")));
    }
    audit(
        pool,
        admin.0.key_id,
        "api_key.revoked",
        serde_json::json!({ "id": id }),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(serde_json::json!({ "revoked": id })))
}
