//! Signed price quotes (Phase 4): checkout can honour a quoted price
//! without recomputing it — integrity comes from an HMAC-SHA256 tag, and
//! staleness from a short TTL. Tampering with the payload or outliving
//! the TTL both invalidate the token; the client then simply re-quotes.

use axum::Json;
use axum::extract::State;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use hmac::{Hmac, Mac};
use lulan_pricing::rules::Quote;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::error::ApiError;
use crate::pricing::{PriceableItem, price_items};
use crate::state::AppState;

type HmacSha256 = Hmac<Sha256>;

pub const QUOTE_TTL_SECONDS: i64 = 300;

/// The signed payload. Items are matched back to order requests by
/// (unit_code, origin, destination, quantity).
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteToken {
    pub trip_id: Uuid,
    pub currency: String,
    pub total_minor: i64,
    /// Unix seconds after which the quote is dead.
    pub exp: i64,
    pub items: Vec<QuoteTokenItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteTokenItem {
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub quantity: i32,
    pub price_minor: i64,
}

pub fn sign(secret: &[u8], token: &QuoteToken) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(token).expect("token serializes"));
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(payload.as_bytes());
    let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{tag}")
}

/// Constant-time verification + TTL check. `None` = reject (tampered,
/// malformed, or expired) — callers re-quote rather than distinguish.
pub fn verify(secret: &[u8], token: &str) -> Option<QuoteToken> {
    let (payload, tag) = token.split_once('.')?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    let tag_bytes = URL_SAFE_NO_PAD.decode(tag).ok()?;
    mac.verify_slice(&tag_bytes).ok()?;
    let decoded: QuoteToken =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (decoded.exp > Utc::now().timestamp()).then_some(decoded)
}

#[derive(Deserialize)]
pub struct QuoteRequest {
    trip_id: Uuid,
    #[serde(default)]
    promo_code: Option<String>,
    items: Vec<PriceableItem>,
}

#[derive(Serialize)]
pub struct QuoteResponse {
    trip_id: Uuid,
    currency: String,
    items: Vec<QuotedItem>,
    total_minor: i64,
    expires_at: chrono::DateTime<Utc>,
    /// Present this at POST /v1/orders to buy at exactly these prices.
    quote_token: String,
}

#[derive(Serialize)]
pub struct QuotedItem {
    unit_code: String,
    origin: String,
    destination: String,
    quantity: i32,
    #[serde(flatten)]
    quote: Quote,
}

/// POST /v1/quotes
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResponse>, ApiError> {
    if req.items.is_empty() {
        return Err(ApiError::BadRequest("quote needs at least one item".into()));
    }
    let priced = price_items(&state, req.trip_id, &req.items, req.promo_code.as_deref()).await?;

    let currency = priced[0].quote.currency.clone();
    let total_minor: i64 = priced.iter().map(|p| p.quote.total_minor).sum();
    let exp = Utc::now().timestamp() + QUOTE_TTL_SECONDS;

    let token = QuoteToken {
        trip_id: req.trip_id,
        currency: currency.clone(),
        total_minor,
        exp,
        items: priced
            .iter()
            .map(|p| QuoteTokenItem {
                unit_code: p.unit_code.clone(),
                origin: p.origin.clone(),
                destination: p.destination.clone(),
                quantity: p.quantity,
                price_minor: p.quote.total_minor,
            })
            .collect(),
    };
    let quote_token = sign(&state.quote_secret, &token);

    Ok(Json(QuoteResponse {
        trip_id: req.trip_id,
        currency,
        items: priced
            .into_iter()
            .map(|p| QuotedItem {
                unit_code: p.unit_code,
                origin: p.origin,
                destination: p.destination,
                quantity: p.quantity,
                quote: p.quote,
            })
            .collect(),
        total_minor,
        expires_at: chrono::DateTime::from_timestamp(exp, 0).expect("valid timestamp"),
        quote_token,
    }))
}
