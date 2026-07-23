//! Customer identity (Phase 6): the core verifies identity, it never owns
//! accounts. [`IdentityProvider`] is a port like `PaymentProvider` — the
//! storefront's IdP issues tokens, Lulan validates them and keeps only a
//! `(issuer, subject)` reference per customer.
//!
//! Two adapters ship:
//!
//! - **JWKS** (`LULAN_IDP_ISSUER` + `LULAN_IDP_JWKS_URL`) — one adapter
//!   covers Auth0, Clerk, Keycloak, Supabase, Firebase, Entra and anything
//!   else that publishes a JWK Set, which is nearly every hosted IdP.
//! - **HS256 shared secret** (`LULAN_IDP_ISSUER` + `LULAN_IDP_HS256_SECRET`)
//!   — enough for a first-party storefront and for tests.
//!
//! JWKS is preferred when both are configured: asymmetric keys mean Lulan
//! never holds anything that could mint a token, only verify one.
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
use std::sync::{Arc, RwLock};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk};
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

/// JWKS-backed verification (RS256/RS384/RS512, ES256/ES384).
///
/// Keys are refreshed by a background task rather than fetched during
/// verification: an HTTP call on the authentication path would put a
/// third party's availability in front of every booking, and a slow IdP
/// would become a slow checkout. A token whose `kid` is not in the cached
/// set is rejected; the next refresh picks up newly published keys.
pub struct JwksIdentity {
    issuer: String,
    audience: Option<String>,
    keys: Arc<RwLock<jwk::JwkSet>>,
}

impl JwksIdentity {
    /// From `LULAN_IDP_ISSUER` + `LULAN_IDP_JWKS_URL`, if both are set.
    /// Optional `LULAN_IDP_AUDIENCE` pins the `aud` claim.
    ///
    /// Fetches once before returning so a misconfigured URL fails at boot
    /// rather than at a customer's first sign-in, then keeps a refresher
    /// running for the process lifetime.
    pub async fn from_env() -> Option<Result<Self, String>> {
        let issuer = std::env::var("LULAN_IDP_ISSUER").ok()?;
        let url = std::env::var("LULAN_IDP_JWKS_URL").ok()?;
        Some(Self::connect(issuer, url, std::env::var("LULAN_IDP_AUDIENCE").ok()).await)
    }

    pub async fn connect(
        issuer: String,
        jwks_url: String,
        audience: Option<String>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        let initial = fetch_jwks(&client, &jwks_url)
            .await
            .map_err(|e| format!("could not fetch {jwks_url}: {e}"))?;
        if initial.keys.is_empty() {
            return Err(format!("{jwks_url} published an empty key set"));
        }
        tracing::info!(keys = initial.keys.len(), %jwks_url, "identity: JWKS loaded");

        let keys = Arc::new(RwLock::new(initial));
        let refresh_secs = std::env::var("LULAN_IDP_JWKS_REFRESH_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        tokio::spawn(refresh_jwks(
            client,
            jwks_url,
            keys.clone(),
            std::time::Duration::from_secs(refresh_secs),
        ));

        Ok(Self {
            issuer,
            audience,
            keys,
        })
    }
}

async fn fetch_jwks(client: &reqwest::Client, url: &str) -> Result<jwk::JwkSet, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<jwk::JwkSet>()
        .await
        .map_err(|e| e.to_string())
}

/// Keeps the cached set current. A failed refresh keeps the previous keys
/// — an IdP blip must not log every customer out.
async fn refresh_jwks(
    client: reqwest::Client,
    url: String,
    keys: Arc<RwLock<jwk::JwkSet>>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the initial fetch already happened
    loop {
        ticker.tick().await;
        match fetch_jwks(&client, &url).await {
            Ok(set) if !set.keys.is_empty() => *keys.write().unwrap() = set,
            Ok(_) => tracing::warn!(%url, "JWKS refresh returned no keys; keeping the last set"),
            Err(err) => {
                tracing::warn!(%url, error = %err, "JWKS refresh failed; keeping the last set")
            }
        }
    }
}

/// Whether a JWT algorithm belongs to the same family as the JWK's key
/// material. Blocks using a key of one type to verify a token signed with
/// an algorithm of another (RSA key ↔ HS256, etc.).
fn alg_family_matches(alg: Algorithm, key: &jwk::AlgorithmParameters) -> bool {
    use jwk::AlgorithmParameters as P;
    match key {
        P::RSA(_) => matches!(
            alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        ),
        P::EllipticCurve(_) => matches!(alg, Algorithm::ES256 | Algorithm::ES384),
        P::OctetKeyPair(_) => matches!(alg, Algorithm::EdDSA),
        P::OctetKey(_) => matches!(alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512),
    }
}

impl IdentityProvider for JwksIdentity {
    fn verify(&self, token: &str) -> Option<Subject> {
        let header = jsonwebtoken::decode_header(token).ok()?;
        let kid = header.kid?;

        let (key, algorithm) = {
            let keys = self.keys.read().ok()?;
            let jwk = keys.find(&kid)?;
            // Trust the key's own algorithm, never the token header's:
            // taking `alg` from the token is how algorithm-confusion
            // attacks start.
            let algorithm = match &jwk.common.key_algorithm {
                Some(alg) => alg.to_string().parse().ok()?,
                // A JWK MAY omit `alg` (spec-legal), so this path is
                // reachable. Only accept the header's alg if its family
                // matches the key material — an RSA public key must not be
                // used to "verify" an HS256 token (classic alg-confusion).
                // Without this the safety rests on jsonwebtoken's internal
                // type check; make it our own invariant instead.
                None if alg_family_matches(header.alg, &jwk.algorithm) => header.alg,
                None => return None,
            };
            (DecodingKey::from_jwk(jwk).ok()?, algorithm)
        };

        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.issuer]);
        match &self.audience {
            Some(audience) => validation.set_audience(&[audience]),
            None => validation.validate_aud = false,
        }
        let data = jsonwebtoken::decode::<JwtClaims>(token, &key, &validation).ok()?;
        Some(Subject {
            issuer: data.claims.iss,
            subject: data.claims.sub,
            email: data.claims.email,
        })
    }
}

/// The configured identity provider, if any. JWKS is preferred: with an
/// asymmetric key Lulan can only verify tokens, never mint them.
pub async fn provider_from_env() -> Option<Arc<dyn IdentityProvider>> {
    match JwksIdentity::from_env().await {
        Some(Ok(provider)) => return Some(Arc::new(provider)),
        Some(Err(err)) => {
            tracing::error!(error = %err, "identity: JWKS configured but unusable");
            return None;
        }
        None => {}
    }
    HsJwtIdentity::from_env().map(|provider| {
        tracing::info!("identity: HS256 shared-secret provider configured");
        Arc::new(provider) as Arc<dyn IdentityProvider>
    })
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
    trip_ids: Vec<Uuid>,
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
        "SELECT id, status, total_minor, currency, created_at,
                (SELECT coalesce(array_agg(DISTINCT oi.trip_id), '{}')
                 FROM order_items oi WHERE oi.order_id = orders.id) AS trip_ids
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
                trip_ids: row.try_get("trip_ids")?,
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
