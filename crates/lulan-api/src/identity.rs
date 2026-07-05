//! Customer identity (Phase 6): the core verifies identity, it never owns
//! accounts. [`IdentityProvider`] is a port like `PaymentProvider` — the
//! storefront's IdP issues tokens, Lulan validates them and keeps only a
//! `(issuer, subject)` reference per customer.
//!
//! Shipped adapter: HS256 JWT with a shared secret (`LULAN_IDP_ISSUER` +
//! `LULAN_IDP_HS256_SECRET`) — enough for first-party storefronts and for
//! tests. An RS256/JWKS adapter (Auth0, Clerk, Keycloak, Supabase) slots
//! in behind the same trait.
//!
//! Guest checkout stays first-class: unauthenticated orders carry a
//! `guest_contact` and get an HMAC retrieval token (same pattern as quote
//! tokens) — effectively a magic link for order lookup and later claiming.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// A verified identity assertion from the operator's IdP.
#[derive(Debug, Clone)]
pub struct Subject {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
}

/// The identity port. Verifies a presented bearer token; never stores
/// credentials.
pub trait IdentityProvider: Send + Sync + 'static {
    fn verify(&self, token: &str) -> Option<Subject>;
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    iss: String,
    sub: String,
    #[serde(default)]
    email: Option<String>,
}

/// HS256 shared-secret JWT validation.
pub struct HsJwtIdentity {
    issuer: String,
    key: DecodingKey,
}

impl HsJwtIdentity {
    pub fn new(issuer: impl Into<String>, secret: &[u8]) -> Self {
        Self {
            issuer: issuer.into(),
            key: DecodingKey::from_secret(secret),
        }
    }

    /// From `LULAN_IDP_ISSUER` + `LULAN_IDP_HS256_SECRET`, if both set.
    pub fn from_env() -> Option<Self> {
        let issuer = std::env::var("LULAN_IDP_ISSUER").ok()?;
        let secret = std::env::var("LULAN_IDP_HS256_SECRET").ok()?;
        Some(Self::new(issuer, secret.as_bytes()))
    }
}

impl IdentityProvider for HsJwtIdentity {
    fn verify(&self, token: &str) -> Option<Subject> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[&self.issuer]);
        validation.validate_aud = false;
        let data = jsonwebtoken::decode::<JwtClaims>(token, &self.key, &validation).ok()?;
        Some(Subject {
            issuer: data.claims.iss,
            subject: data.claims.sub,
            email: data.claims.email,
        })
    }
}

/// Resolve an optional customer identity from `Authorization: Bearer`.
/// API keys (`llk_…`) are a different credential and are ignored here.
pub fn bearer_subject(state: &AppState, headers: &axum::http::HeaderMap) -> Option<Subject> {
    let identity = state.identity.as_ref()?;
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    if token.starts_with("llk_") {
        return None;
    }
    identity.verify(token)
}

/// Upsert the customer reference for a verified subject.
pub async fn upsert_customer(pool: &PgPool, subject: &Subject) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        INSERT INTO customers (id, issuer, subject, email)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (issuer, subject)
            DO UPDATE SET email = coalesce(excluded.email, customers.email)
        RETURNING id
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(&subject.issuer)
    .bind(&subject.subject)
    .bind(&subject.email)
    .fetch_one(pool)
    .await
}

pub async fn find_customer(pool: &PgPool, subject: &Subject) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM customers WHERE issuer = $1 AND subject = $2")
        .bind(&subject.issuer)
        .bind(&subject.subject)
        .fetch_optional(pool)
        .await
}

// ---- Guest retrieval tokens ------------------------------------------

/// HMAC retrieval token for guest orders (domain-separated from quote
/// tokens by the context prefix).
pub fn retrieval_token(secret: &[u8], order_id: Uuid) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(b"order-retrieval:");
    mac.update(order_id.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn verify_retrieval_token(secret: &[u8], order_id: Uuid, token: &str) -> bool {
    let expected = retrieval_token(secret, order_id);
    expected.len() == token.len()
        && expected
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

// ---- Customer endpoints ----------------------------------------------

/// The authenticated customer, as an extractor.
pub struct CustomerAuth {
    pub customer_id: Uuid,
}

impl axum::extract::FromRequestParts<AppState> for CustomerAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let pool = state
            .db
            .as_ref()
            .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
        let subject = bearer_subject(state, &parts.headers)
            .ok_or(ApiError::Unauthorized("customer bearer token required"))?;
        let customer_id = upsert_customer(pool, &subject)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        Ok(CustomerAuth { customer_id })
    }
}

#[derive(Serialize)]
pub struct CustomerOrder {
    order_id: Uuid,
    trip_id: Uuid,
    status: String,
    total_minor: i64,
    currency: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// GET /v1/customers/me/orders — the authenticated customer's bookings.
pub async fn my_orders(
    State(state): State<AppState>,
    customer: CustomerAuth,
) -> Result<Json<Vec<CustomerOrder>>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    let rows = sqlx::query(
        "SELECT id, trip_id, status, total_minor, currency, created_at
         FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(customer.customer_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let orders = rows
        .into_iter()
        .map(|row| {
            Ok(CustomerOrder {
                order_id: row.try_get("id")?,
                trip_id: row.try_get("trip_id")?,
                status: row.try_get("status")?,
                total_minor: row.try_get("total_minor")?,
                currency: row.try_get("currency")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(orders))
}

#[derive(Deserialize)]
pub struct ClaimRequest {
    retrieval_token: String,
}

/// POST /v1/orders/{id}/claim — attach a guest order to the authenticated
/// customer (guest-books-then-signs-up).
pub async fn claim_order(
    State(state): State<AppState>,
    customer: CustomerAuth,
    Path(order_id): Path<Uuid>,
    Json(req): Json<ClaimRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state
        .db
        .as_ref()
        .ok_or(ApiError::ServiceUnavailable("database not configured"))?;
    if !verify_retrieval_token(&state.quote_secret, order_id, &req.retrieval_token) {
        return Err(ApiError::Forbidden("invalid retrieval token"));
    }
    let updated = sqlx::query(
        "UPDATE orders SET customer_id = $2, updated_at = now()
         WHERE id = $1 AND customer_id IS NULL",
    )
    .bind(order_id)
    .bind(customer.customer_id)
    .execute(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::Conflict(
            "order not found or already claimed".into(),
        ));
    }
    Ok(Json(
        serde_json::json!({ "order_id": order_id, "customer_id": customer.customer_id }),
    ))
}
