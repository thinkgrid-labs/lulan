//! Order lifecycle endpoints: create (prices, then atomically claims
//! inventory for N passengers), request payment, provider webhook (which
//! auto-issues tickets on capture), cancel, fetch.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::NaiveDate;
use lulan_engine::domain::{OrderStatus, PassengerType};
use lulan_engine::events::StoredEvent;
use lulan_engine::orders::{
    CreateOutcome, ItemValidation, NewOrderItem, NewPassenger, OrderRecord, TransitionOutcome,
};
use lulan_engine::payments::{FakeProvider, PaymentProvider};
use lulan_engine::ticket::TicketStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_PASSENGERS: usize = 20;

#[derive(Deserialize)]
pub struct CreateOrderRequest {
    trip_id: Uuid,
    /// The travelling party; passengers[0] is the lead/contact.
    passengers: Vec<PassengerRequest>,
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
pub struct PassengerRequest {
    full_name: String,
    #[serde(rename = "type")]
    passenger_type: String,
    #[serde(default)]
    birthdate: Option<NaiveDate>,
}

#[derive(Deserialize)]
pub struct OrderItemRequest {
    unit_code: String,
    origin: String,
    destination: String,
    #[serde(default)]
    quantity: Option<i32>,
    /// Index into `passengers`. Required for seat items unless there is
    /// exactly one passenger; must be absent for pool items.
    #[serde(default)]
    passenger: Option<usize>,
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
    if req.passengers.is_empty() || req.passengers.len() > MAX_PASSENGERS {
        return Err(ApiError::BadRequest(format!(
            "orders need 1–{MAX_PASSENGERS} passengers"
        )));
    }
    let orders = state.orders()?;
    let inventory = state.inventory()?;

    let mut passengers = Vec::with_capacity(req.passengers.len());
    for p in &req.passengers {
        if p.full_name.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "passenger full_name is required".into(),
            ));
        }
        let passenger_type = PassengerType::parse(&p.passenger_type).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "unknown passenger type {:?} (adult/child/senior/pwd/infant)",
                p.passenger_type
            ))
        })?;
        passengers.push(NewPassenger {
            full_name: p.full_name.trim().to_string(),
            passenger_type,
            birthdate: p.birthdate,
        });
    }

    // The passenger type a seat item travels under (index rules mirror the
    // store's: a single passenger is the default for index-less seats).
    let seat_passenger_type = |item: &OrderItemRequest| -> Option<String> {
        let index = item.passenger.or((passengers.len() == 1).then_some(0))?;
        passengers
            .get(index)
            .map(|p| p.passenger_type.as_str().to_string())
    };

    let items: Vec<NewOrderItem> = match &req.quote_token {
        Some(token) => {
            // Honour quoted prices — untampered, unexpired, same trip,
            // covering every requested item including its passenger type.
            let quote = crate::quotes::verify(&state.quote_secret, token)
                .ok_or_else(|| ApiError::BadRequest("invalid or expired quote token".into()))?;
            if quote.trip_id != req.trip_id {
                return Err(ApiError::BadRequest(
                    "quote token is for a different trip".into(),
                ));
            }
            let mut items = Vec::with_capacity(req.items.len());
            for item in &req.items {
                let quantity = item.quantity.unwrap_or(1);
                let target = inventory
                    .resolve_target(
                        req.trip_id,
                        &item.unit_code,
                        &item.origin,
                        &item.destination,
                    )
                    .await?
                    .ok_or_else(|| {
                        ApiError::NotFound(format!(
                            "trip {} with unit {:?}",
                            req.trip_id, item.unit_code
                        ))
                    })?;
                let passenger_type = if target.kind == "seat" {
                    seat_passenger_type(item)
                } else {
                    None
                };
                let quoted = quote
                    .items
                    .iter()
                    .find(|q| {
                        q.unit_code == item.unit_code
                            && q.origin == item.origin
                            && q.destination == item.destination
                            && q.quantity == quantity
                            && q.passenger_type == passenger_type
                    })
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!(
                            "item {} ({}→{} ×{quantity}, {:?}) is not covered by the quote",
                            item.unit_code, item.origin, item.destination, passenger_type
                        ))
                    })?;
                items.push(NewOrderItem {
                    unit_code: item.unit_code.clone(),
                    origin: item.origin.clone(),
                    destination: item.destination.clone(),
                    quantity,
                    price_minor: quoted.price_minor,
                    passenger_index: item.passenger,
                });
            }
            items
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
                    passenger_type: seat_passenger_type(i),
                })
                .collect();
            crate::pricing::price_items(&state, req.trip_id, &priceable, req.promo_code.as_deref())
                .await?
                .into_iter()
                .zip(&req.items)
                .map(|(p, i)| NewOrderItem {
                    unit_code: p.unit_code,
                    origin: p.origin,
                    destination: p.destination,
                    quantity: p.quantity,
                    price_minor: p.quote.total_minor,
                    passenger_index: i.passenger,
                })
                .collect()
        }
    };

    match orders.create(req.trip_id, &passengers, &items).await? {
        CreateOutcome::Created(record) => {
            // Consumed holds have served their purpose; best-effort cleanup.
            if let Ok(holds) = state.holds() {
                for hold_id in req.items.iter().filter_map(|i| i.hold_id) {
                    let _ = holds.release(hold_id).await;
                }
            }
            Ok((StatusCode::CREATED, Json(record)))
        }
        CreateOutcome::Conflict { unit_code } => Err(ApiError::Conflict(format!(
            "{unit_code} is no longer available for the requested span"
        ))),
        CreateOutcome::NotFound { what } => Err(ApiError::NotFound(format!("{what} not found"))),
        CreateOutcome::Invalid(validation) => Err(ApiError::BadRequest(match validation {
            ItemValidation::SeatNeedsPassenger { unit_code } => {
                format!("seat {unit_code} needs a passenger index")
            }
            ItemValidation::PassengerIndexOutOfRange { unit_code, index } => {
                format!("seat {unit_code} references passenger {index}, which does not exist")
            }
            ItemValidation::PoolWithPassenger { unit_code } => {
                format!("pool item {unit_code} cannot reference a passenger")
            }
            ItemValidation::NoPassengers => "orders need at least one passenger".into(),
        })),
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
/// A successful capture auto-issues tickets (best-effort: the order stays
/// Paid and re-issuable if ticketing hiccups).
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
    let outcome = orders
        .apply_payment_result(&req.payment_intent_id, succeeded)
        .await?;

    let mut order_status = match &outcome {
        TransitionOutcome::Applied(status) | TransitionOutcome::NoOp(status) => *status,
        TransitionOutcome::NotFound => {
            return Err(ApiError::NotFound(format!(
                "no order for payment intent {:?}",
                req.payment_intent_id
            )));
        }
    };

    if matches!(outcome, TransitionOutcome::Applied(OrderStatus::Paid))
        && let (Some(pool), Some(signer)) = (&state.db, &state.ticket_signer)
        && let Some(order_id) = orders.find_by_intent(&req.payment_intent_id).await?
    {
        match TicketStore::new(pool.clone())
            .issue_for_order(order_id, signer)
            .await
        {
            Ok(tickets) => {
                tracing::info!(%order_id, count = tickets.len(), "tickets issued");
                order_status = OrderStatus::Ticketed;
            }
            Err(err) => {
                tracing::error!(%order_id, error = %err, "ticket issuance failed — order stays paid");
            }
        }
    }

    Ok(Json(WebhookResponse {
        order_status,
        applied: matches!(outcome, TransitionOutcome::Applied(_)),
    }))
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
