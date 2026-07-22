//! `Idempotency-Key` handling for booking retries.
//!
//! A booking retry must never produce a second booking. Two properties do
//! that, and both were missing before:
//!
//! 1. **The key is reserved before the order is created**, not recorded
//!    after. Reading first and writing at the end leaves a window where two
//!    concurrent retries both find nothing and both book — the failure mode
//!    the feature exists to prevent, arriving exactly when it matters (a
//!    double-click, a client-side retry storm). The reservation is an
//!    `INSERT … ON CONFLICT DO NOTHING`, so the database picks the winner.
//!
//! 2. **The key is scoped to the caller and bound to the request.** A bare
//!    `key` column is a global namespace: a client sending
//!    `Idempotency-Key: 1` would be handed back whatever order the previous
//!    `1` created — including its passenger names and its `retrieval_token`,
//!    which is the credential for reading and claiming that booking. Keys
//!    now live under `(scope, key)`, and a key replayed with a different
//!    body is refused rather than answered with an unrelated order.
//!
//! Lifecycle: `replay_if_completed` (cheap, before pricing) → `reserve`
//! (immediately before the write) → `complete` on success, `release` on
//! failure so a legitimate retry isn't blocked by an order that never
//! existed. A process that dies in between leaves a `pending` row, which
//! `OrderStore::sweep_idempotency_keys` reclaims.

use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;

/// A key this request owns and must resolve.
#[derive(Debug, Clone)]
pub struct Reservation {
    scope: String,
    key: String,
}

/// What a reservation attempt resolved to.
pub enum Reserved {
    /// No `Idempotency-Key` header; the caller opted out.
    Disabled,
    /// The key is ours: create the order, then `complete` or `release`.
    Held(Reservation),
    /// A previous request under this key already produced this response.
    Replay(StatusCode, serde_json::Value),
}

/// Who a key belongs to. Guests are keyed by a hash of their contact
/// rather than the contact itself: this column is only ever compared, so
/// there is no reason to keep a second copy of the address.
pub fn scope(customer_id: Option<Uuid>, guest_contact: Option<&str>) -> String {
    match (customer_id, guest_contact) {
        (Some(id), _) => format!("customer:{id}"),
        (None, Some(contact)) => {
            let normalised = contact.trim().to_lowercase();
            let digest = Sha256::digest(normalised.as_bytes());
            format!("guest:{}", URL_SAFE_NO_PAD.encode(digest))
        }
        // Unreachable via the handler, which requires one or the other.
        (None, None) => "anonymous".to_string(),
    }
}

/// Fingerprint of the request this key stands for. Hashing the parsed
/// request rather than the raw bytes keeps a re-serialised retry (different
/// whitespace, same meaning) matching.
pub fn request_hash<T: Serialize>(request: &T) -> Vec<u8> {
    let canonical = serde_json::to_vec(request).unwrap_or_default();
    Sha256::digest(&canonical).to_vec()
}

/// Header value, if the caller sent one.
pub fn key_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// Fast path: a completed response for this exact key and request, before
/// doing any pricing work. In-flight and mismatched keys are left for
/// [`reserve`] to adjudicate at the point of the write.
pub async fn replay_if_completed(
    pool: &PgPool,
    key: Option<&str>,
    scope: &str,
    request_hash: &[u8],
) -> Result<Option<(StatusCode, serde_json::Value)>, ApiError> {
    let Some(key) = key else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT status_code, response, request_hash FROM idempotency_keys
         WHERE scope = $1 AND key = $2 AND status = 'completed'",
    )
    .bind(scope)
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let Some(row) = row else { return Ok(None) };
    let stored_hash: Vec<u8> = row
        .try_get("request_hash")
        .map_err(|e| ApiError::Internal(e.into()))?;
    if stored_hash != request_hash {
        return Err(reuse_conflict());
    }
    Ok(Some(stored_response(&row)?))
}

/// Claim the key. Call immediately before creating the order: everything
/// between this and [`complete`]/[`release`] is a window in which a
/// concurrent retry sees `pending`.
pub async fn reserve(
    pool: &PgPool,
    key: Option<String>,
    scope: String,
    request_hash: &[u8],
) -> Result<Reserved, ApiError> {
    let Some(key) = key else {
        return Ok(Reserved::Disabled);
    };

    let claimed = sqlx::query(
        "INSERT INTO idempotency_keys (scope, key, request_hash)
         VALUES ($1, $2, $3) ON CONFLICT (scope, key) DO NOTHING",
    )
    .bind(&scope)
    .bind(&key)
    .bind(request_hash)
    .execute(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
    .rows_affected();
    if claimed == 1 {
        return Ok(Reserved::Held(Reservation { scope, key }));
    }

    // Someone else got there first.
    let row = sqlx::query(
        "SELECT status, status_code, response, request_hash FROM idempotency_keys
         WHERE scope = $1 AND key = $2",
    )
    .bind(&scope)
    .bind(&key)
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    // Swept between the insert and the read — treat as a fresh attempt.
    let Some(row) = row else {
        return Ok(Reserved::Held(Reservation { scope, key }));
    };

    let stored_hash: Vec<u8> = row
        .try_get("request_hash")
        .map_err(|e| ApiError::Internal(e.into()))?;
    if stored_hash != request_hash {
        return Err(reuse_conflict());
    }
    let status: String = row
        .try_get("status")
        .map_err(|e| ApiError::Internal(e.into()))?;
    if status == "completed" {
        let (code, body) = stored_response(&row)?;
        return Ok(Reserved::Replay(code, body));
    }
    Err(ApiError::Conflict(
        "a booking with this Idempotency-Key is already in flight — retry shortly to \
         receive its result"
            .into(),
    ))
}

/// Store the response so retries replay it verbatim.
pub async fn complete(
    pool: &PgPool,
    reservation: &Reservation,
    order_id: Uuid,
    status: StatusCode,
    body: &serde_json::Value,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE idempotency_keys
         SET status = 'completed', order_id = $3, status_code = $4, response = $5
         WHERE scope = $1 AND key = $2",
    )
    .bind(&reservation.scope)
    .bind(&reservation.key)
    .bind(order_id)
    .bind(status.as_u16() as i32)
    .bind(body)
    .execute(pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(())
}

/// Give the key back. Nothing was booked, so the caller must be free to
/// retry with the same key — after picking a different seat, say.
pub async fn release(pool: &PgPool, reservation: &Reservation) {
    let result = sqlx::query(
        "DELETE FROM idempotency_keys WHERE scope = $1 AND key = $2 AND status = 'pending'",
    )
    .bind(&reservation.scope)
    .bind(&reservation.key)
    .execute(pool)
    .await;
    if let Err(err) = result {
        // The sweeper reclaims it; the client is only blocked until then.
        tracing::warn!(error = %err, "releasing an idempotency reservation failed");
    }
}

fn reuse_conflict() -> ApiError {
    ApiError::Conflict(
        "Idempotency-Key was already used for a different request — use a fresh key for a \
         new booking"
            .into(),
    )
}

fn stored_response(
    row: &sqlx::postgres::PgRow,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let code: Option<i32> = row
        .try_get("status_code")
        .map_err(|e| ApiError::Internal(e.into()))?;
    let body: Option<serde_json::Value> = row
        .try_get("response")
        .map_err(|e| ApiError::Internal(e.into()))?;
    match (code, body) {
        (Some(code), Some(body)) => Ok((
            StatusCode::from_u16(code as u16).unwrap_or(StatusCode::OK),
            body,
        )),
        _ => Err(ApiError::Internal(anyhow::anyhow!(
            "completed idempotency record is missing its response"
        ))),
    }
}
