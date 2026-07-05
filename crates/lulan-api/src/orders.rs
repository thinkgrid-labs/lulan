//! Order lifecycle endpoints (Phase 3): create (atomically claims
//! inventory), request payment, provider webhook, cancel, fetch.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lulan_engine::domain::OrderStatus;
use lulan_engine::events::StoredEvent;
use lulan_engine::orders::{CreateOutcome, NewOrderItem, OrderRecord, TransitionOutcome};
use lulan_engine::payments::{FakeProvider, PaymentProvider};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateOrderRequest {
    trip_id: Uuid,
    passenger_name: String,
    /// Buy at previously quoted prices (see POST /v1/quotes). Without it,
    /// items are priced live at order time.
    #[serde(default)]
    quote_token: Option<String>,
    /// Live-pricing only; quoted orders already have promos baked in.
    #[serde(default)]
    promo_code: Option<String>,
    items: Vec<OrderItemRequest>,
}

#[derive(Deserialize)]
pub struct OrderItemRequest {
    unit_code: String,
    origin: String,
    destination: String,
    #[serde(default)]
    quantity: Option<i32>,
    #[serde(default)]
    hold_id: Option<Uuid>,
}

/// POST /v1/orders — prices, then claims all items and creates the order
/// atomically.
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<OrderRecord>), ApiError> {
    if req.items.is_empty() {
        return Err(ApiError::BadRequest("order needs at least one item".into()));
    }
    if req.passenger_name.trim().is_empty() {
        return Err(ApiError::BadRequest("passenger_name is required".into()));
    }
    let orders = state.orders()?;

    let items: Vec<NewOrderItem> = match &req.quote_token {
        Some(token) => {
            // Honour quoted prices — but only from an untampered, unexpired
            // token for this trip that covers every requested item.
            let quote = crate::quotes::verify(&state.quote_secret, token)
                .ok_or_else(|| ApiError::BadRequest("invalid or expired quote token".into()))?;
            if quote.trip_id != req.trip_id {
                return Err(ApiError::BadRequest(
                    "quote token is for a different trip".into(),
                ));
            }
            req.items
                .iter()
                .map(|i| {
                    let quantity = i.quantity.unwrap_or(1);
                    let quoted = quote
                        .items
                        .iter()
                        .find(|q| {
                            q.unit_code == i.unit_code
                                && q.origin == i.origin
                                && q.destination == i.destination
                                && q.quantity == quantity
                        })
                        .ok_or_else(|| {
                            ApiError::BadRequest(format!(
                                "item {} ({}→{} ×{quantity}) is not covered by the quote",
                                i.unit_code, i.origin, i.destination
                            ))
                        })?;
                    Ok(NewOrderItem {
                        unit_code: i.unit_code.clone(),
                        origin: i.origin.clone(),
                        destination: i.destination.clone(),
                        quantity,
                        price_minor: quoted.price_minor,
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?
        }
        None => {
            let priceable: Vec<crate::pricing::PriceableItem> = req
                .items
                .iter()
                .map(|i| crate::pricing::PriceableItem {
                    unit_code: i.unit_code.clone(),
                    origin: i.origin.clone(),
                    destination: i.destination.clone(),
                    quantity: i.quantity,
                })
                .collect();
            crate::pricing::price_items(&state, req.trip_id, &priceable, req.promo_code.as_deref())
                .await?
                .into_iter()
                .map(|p| NewOrderItem {
                    unit_code: p.unit_code,
                    origin: p.origin,
                    destination: p.destination,
                    quantity: p.quantity,
                    price_minor: p.quote.total_minor,
                })
                .collect()
        }
    };

    match orders
        .create(req.trip_id, req.passenger_name.trim(), &items)
        .await?
    {
        CreateOutcome::Created(record) => {
            // Consumed holds have served their purpose; best-effort cleanup.
            if let Ok(holds) = state.holds() {
                for item in req.items.iter().filter_map(|i| i.hold_id) {
                    let _ = holds.release(item).await;
                }
            }
            Ok((StatusCode::CREATED, Json(record)))
        }
        CreateOutcome::Conflict { unit_code } => Err(ApiError::Conflict(format!(
            "{unit_code} is no longer available for the requested span"
        ))),
        CreateOutcome::NotFound { what } => Err(ApiError::NotFound(format!("{what} not found"))),
    }
}

#[derive(Serialize)]
pub struct OrderDetails {
    #[serde(flatten)]
    record: OrderRecord,
    events: Vec<EventSummary>,
}

#[derive(Serialize)]
pub struct EventSummary {
    stream_seq: i32,
    event_type: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

/// GET /v1/orders/{order_id}
pub async fn get(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<OrderDetails>, ApiError> {
    let orders = state.orders()?;
    let record = orders
        .get(order_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("order {order_id} not found")))?;
    let events = lulan_engine::events::stream(state.db.as_ref().unwrap(), order_id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?
        .into_iter()
        .map(|e: StoredEvent| EventSummary {
            stream_seq: e.stream_seq,
            event_type: e.event_type,
            occurred_at: e.occurred_at,
        })
        .collect();
    Ok(Json(OrderDetails { record, events }))
}

#[derive(Serialize)]
pub struct PaymentResponse {
    order_id: Uuid,
    status: OrderStatus,
    payment_intent_id: String,
}

/// POST /v1/orders/{order_id}/payment — Locked → PendingPayment via the
/// configured provider (FakeProvider until a real adapter lands).
pub async fn request_payment(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<PaymentResponse>, ApiError> {
    let orders = state.orders()?;
    let record = orders
        .get(order_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("order {order_id} not found")))?;

    let provider = FakeProvider;
    let intent = provider
        .create_intent(order_id, record.total_minor, &record.currency)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    match orders.request_payment(order_id, &intent.id).await? {
        TransitionOutcome::Applied(status) => Ok(Json(PaymentResponse {
            order_id,
            status,
            payment_intent_id: intent.id,
        })),
        TransitionOutcome::NoOp(current) => Err(ApiError::Conflict(format!(
            "payment cannot be requested in state {:?}",
            current.as_str()
        ))),
        TransitionOutcome::NotFound => {
            Err(ApiError::NotFound(format!("order {order_id} not found")))
        }
    }
}

#[derive(Deserialize)]
pub struct FakeWebhookRequest {
    payment_intent_id: String,
    /// `succeeded` or `failed` — mirrors real provider webhook vocabulary.
    status: String,
}

#[derive(Serialize)]
pub struct WebhookResponse {
    order_status: OrderStatus,
    /// False when the delivery was a duplicate or out of order — the call
    /// still returns 200 so providers stop retrying (idempotency).
    applied: bool,
}

/// POST /v1/payments/fake/webhook — the FakeProvider's async notification.
pub async fn fake_webhook(
    State(state): State<AppState>,
    Json(req): Json<FakeWebhookRequest>,
) -> Result<Json<WebhookResponse>, ApiError> {
    let succeeded = match req.status.as_str() {
        "succeeded" => true,
        "failed" => false,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown payment status {other:?}"
            )));
        }
    };
    let orders = state.orders()?;
    match orders
        .apply_payment_result(&req.payment_intent_id, succeeded)
        .await?
    {
        TransitionOutcome::Applied(status) => Ok(Json(WebhookResponse {
            order_status: status,
            applied: true,
        })),
        TransitionOutcome::NoOp(status) => Ok(Json(WebhookResponse {
            order_status: status,
            applied: false,
        })),
        TransitionOutcome::NotFound => Err(ApiError::NotFound(format!(
            "no order for payment intent {:?}",
            req.payment_intent_id
        ))),
    }
}

#[derive(Serialize)]
pub struct CancelResponse {
    order_id: Uuid,
    status: OrderStatus,
}

/// POST /v1/orders/{order_id}/cancel — releases claims atomically.
pub async fn cancel(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<CancelResponse>, ApiError> {
    let orders = state.orders()?;
    match orders.cancel(order_id).await? {
        TransitionOutcome::Applied(status) => Ok(Json(CancelResponse { order_id, status })),
        TransitionOutcome::NoOp(current) => Err(ApiError::Conflict(format!(
            "order cannot be cancelled in state {:?}",
            current.as_str()
        ))),
        TransitionOutcome::NotFound => {
            Err(ApiError::NotFound(format!("order {order_id} not found")))
        }
    }
}
