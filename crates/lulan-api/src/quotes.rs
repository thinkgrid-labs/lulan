//! Signed price quotes (Phase 4, itinerary-shaped since Phase 6.5):
//! checkout can honour a quoted price without recomputing it — integrity
//! comes from an HMAC-SHA256 tag, and staleness from a short TTL.
//! Tampering with the payload or outliving the TTL both invalidate the
//! token; the client then simply re-quotes.

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
use crate::pricing::{JourneyItem, PriceableItem, journey_context, price_items};
use crate::state::AppState;

type HmacSha256 = Hmac<Sha256>;

pub const QUOTE_TTL_SECONDS: i64 = 300;

/// The signed payload. Items are matched back to order requests by
/// (trip_id, unit_code, origin, destination, quantity, passenger_type).
#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteToken {
    pub currency: String,
    pub total_minor: i64,
    /// Unix seconds after which the quote is dead.
    pub exp: i64,
    pub items: Vec<QuoteTokenItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QuoteTokenItem {
    pub trip_id: Uuid,
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub quantity: i32,
    /// Part of the match key: a senior-priced quote can't buy an adult
    /// seat.
    #[serde(default)]
    pub passenger_type: Option<String>,
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

/// One journey (leg) of an itinerary request.
#[derive(Debug, Deserialize)]
pub struct JourneyRequest {
    pub trip_id: Uuid,
    pub items: Vec<JourneyItem>,
}

/// Itinerary shape (`journeys`) or the single-trip shape (`trip_id` +
/// `items`) — a one-way is just a one-journey itinerary.
#[derive(Deserialize)]
pub struct QuoteRequest {
    #[serde(default)]
    trip_id: Option<Uuid>,
    #[serde(default)]
    items: Option<Vec<JourneyItem>>,
    #[serde(default)]
    journeys: Option<Vec<JourneyRequest>>,
    #[serde(default)]
    promo_code: Option<String>,
}

/// Normalise either request shape into journeys. Shared with orders.
pub fn normalize_journeys(
    trip_id: Option<Uuid>,
    items: Option<Vec<JourneyItem>>,
    journeys: Option<Vec<JourneyRequest>>,
) -> Result<Vec<(Uuid, Vec<JourneyItem>)>, ApiError> {
    match (trip_id, items, journeys) {
        (None, None, Some(journeys)) => {
            if journeys.is_empty() || journeys.iter().any(|j| j.items.is_empty()) {
                return Err(ApiError::BadRequest(
                    "every journey needs at least one item".into(),
                ));
            }
            if journeys.len() > 8 {
                return Err(ApiError::BadRequest("max 8 journeys per itinerary".into()));
            }
            Ok(journeys.into_iter().map(|j| (j.trip_id, j.items)).collect())
        }
        (Some(trip_id), Some(items), None) => {
            if items.is_empty() {
                return Err(ApiError::BadRequest("needs at least one item".into()));
            }
            Ok(vec![(trip_id, items)])
        }
        _ => Err(ApiError::BadRequest(
            "provide either journeys[] or trip_id + items".into(),
        )),
    }
}

#[derive(Serialize)]
pub struct QuoteResponse {
    currency: String,
    journey_count: u32,
    is_round_trip: bool,
    items: Vec<QuotedItem>,
    total_minor: i64,
    expires_at: chrono::DateTime<Utc>,
    /// Present this at POST /v1/orders to buy at exactly these prices.
    quote_token: String,
}

#[derive(Serialize)]
pub struct QuotedItem {
    trip_id: Uuid,
    unit_code: String,
    origin: String,
    destination: String,
    quantity: i32,
    passenger_type: Option<String>,
    #[serde(flatten)]
    quote: Quote,
}

/// POST /v1/quotes
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResponse>, ApiError> {
    let journeys = normalize_journeys(req.trip_id, req.items, req.journeys)?;
    let context = journey_context(&journeys);
    let items: Vec<PriceableItem> = journeys
        .iter()
        .flat_map(|(trip_id, items)| {
            items.iter().map(|i| PriceableItem {
                trip_id: *trip_id,
                unit_code: i.unit_code.clone(),
                origin: i.origin.clone(),
                destination: i.destination.clone(),
                quantity: i.quantity,
                passenger_type: i.passenger_type.clone(),
            })
        })
        .collect();

    let priced = price_items(&state, &items, req.promo_code.as_deref(), context).await?;

    let currency = priced[0].quote.currency.clone();
    let total_minor: i64 = priced.iter().map(|p| p.quote.total_minor).sum();
    let exp = Utc::now().timestamp() + QUOTE_TTL_SECONDS;

    let token = QuoteToken {
        currency: currency.clone(),
        total_minor,
        exp,
        items: priced
            .iter()
            .map(|p| QuoteTokenItem {
                trip_id: p.trip_id,
                unit_code: p.unit_code.clone(),
                origin: p.origin.clone(),
                destination: p.destination.clone(),
                quantity: p.quantity,
                passenger_type: p.passenger_type.clone(),
                price_minor: p.quote.total_minor,
            })
            .collect(),
    };
    let quote_token = sign(&state.quote_secret, &token);

    Ok(Json(QuoteResponse {
        currency,
        journey_count: context.journey_count,
        is_round_trip: context.is_round_trip,
        items: priced
            .into_iter()
            .map(|p| QuotedItem {
                trip_id: p.trip_id,
                unit_code: p.unit_code,
                origin: p.origin,
                destination: p.destination,
                quantity: p.quantity,
                passenger_type: p.passenger_type,
                quote: p.quote,
            })
            .collect(),
        total_minor,
        expires_at: chrono::DateTime::from_timestamp(exp, 0).expect("valid timestamp"),
        quote_token,
    }))
}
