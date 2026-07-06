//! Order lifecycle endpoints: create (prices, then atomically claims
//! inventory for N passengers), request payment, provider webhook (which
//! auto-issues tickets on capture), cancel, fetch.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
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

/// Itinerary shape (`journeys`) or single-trip shape (`trip_id`+`items`).
#[derive(Deserialize)]
pub struct CreateOrderRequest {
    #[serde(default)]
    trip_id: Option<Uuid>,
    #[serde(default)]
    items: Option<Vec<OrderItemRequest>>,
    #[serde(default)]
    journeys: Option<Vec<OrderJourneyRequest>>,
    /// The travelling party; passengers[0] is the lead/contact. One list
    /// spans every journey of the itinerary.
    passengers: Vec<PassengerRequest>,
    /// Guest checkout contact (email or phone). Required unless the
    /// request carries a customer bearer token.
    #[serde(default)]
    guest_contact: Option<String>,
    /// Buy at previously quoted prices (see POST /v1/quotes). Without it,
    /// items are priced live at order time.
    #[serde(default)]
    quote_token: Option<String>,
    /// Live-pricing only; quoted orders already have promos baked in.
    #[serde(default)]
    promo_code: Option<String>,
    /// Itinerary hold from POST /v1/holds. Released once the order's own
    /// claims succeed; a claim is authoritative with or without it.
    #[serde(default)]
    hold_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct OrderJourneyRequest {
    trip_id: Uuid,
    items: Vec<OrderItemRequest>,
}

/// The created order plus its retrieval credential: guests keep the
/// token (magic-link semantics); customers can also list via
/// `/v1/customers/me/orders`.
#[derive(Serialize)]
pub struct CreateOrderResponse {
    #[serde(flatten)]
    record: OrderRecord,
    retrieval_token: String,
    customer_id: Option<Uuid>,
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
}

/// POST /v1/orders — prices, then claims all items and creates the order
/// atomically. Supports `Idempotency-Key` (the stored 201 is replayed for
/// retries) and either a customer bearer token or guest checkout.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Normalise both request shapes into journeys of (trip, items).
    let journeys: Vec<(Uuid, Vec<OrderItemRequest>)> = match (req.trip_id, req.items, req.journeys)
    {
        (None, None, Some(journeys)) => {
            if journeys.is_empty() || journeys.iter().any(|j| j.items.is_empty()) {
                return Err(ApiError::BadRequest(
                    "every journey needs at least one item".into(),
                ));
            }
            if journeys.len() > 8 {
                return Err(ApiError::BadRequest("max 8 journeys per itinerary".into()));
            }
            journeys.into_iter().map(|j| (j.trip_id, j.items)).collect()
        }
        (Some(trip_id), Some(items), None) if !items.is_empty() => vec![(trip_id, items)],
        _ => {
            return Err(ApiError::BadRequest(
                "provide either journeys[] or trip_id + items".into(),
            ));
        }
    };
    if req.passengers.is_empty() || req.passengers.len() > MAX_PASSENGERS {
        return Err(ApiError::BadRequest(format!(
            "orders need 1–{MAX_PASSENGERS} passengers"
        )));
    }
    let orders = state.orders()?;
    let inventory = state.inventory()?;
    let pool = state.db.as_ref().expect("orders() guaranteed db");

    // Booking retries must not double-book: replay the stored response.
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(key) = &idempotency_key {
        let stored: Option<(i32, serde_json::Value)> =
            sqlx::query_as("SELECT status_code, response FROM idempotency_keys WHERE key = $1")
                .bind(key)
                .fetch_optional(pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
        if let Some((status, response)) = stored {
            return Ok((
                StatusCode::from_u16(status as u16).unwrap_or(StatusCode::OK),
                Json(response),
            ));
        }
    }

    // Identity: a customer bearer token, or guest checkout with contact.
    let customer_id = match crate::identity::bearer_subject(&state, &headers) {
        Some(subject) => Some(
            crate::identity::upsert_customer(pool, &subject)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?,
        ),
        None => None,
    };
    let guest_contact = req.guest_contact.as_deref().map(str::trim);
    if customer_id.is_none() && guest_contact.is_none_or(str::is_empty) {
        return Err(ApiError::BadRequest(
            "guest orders need guest_contact (email or phone) for retrieval".into(),
        ));
    }

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

    // Flatten journeys into (trip, item) pairs; derive itinerary context
    // (journey_count, is_round_trip) for pricing.
    let flat: Vec<(Uuid, &OrderItemRequest)> = journeys
        .iter()
        .flat_map(|(trip_id, items)| items.iter().map(move |i| (*trip_id, i)))
        .collect();
    let context = crate::pricing::journey_context(
        &journeys
            .iter()
            .map(|(trip_id, items)| {
                (
                    *trip_id,
                    items
                        .iter()
                        .map(|i| crate::pricing::JourneyItem {
                            unit_code: i.unit_code.clone(),
                            origin: i.origin.clone(),
                            destination: i.destination.clone(),
                            quantity: i.quantity,
                            passenger_type: None,
                        })
                        .collect(),
                )
            })
            .collect::<Vec<_>>(),
    );

    let items: Vec<NewOrderItem> = match &req.quote_token {
        Some(token) => {
            // Honour quoted prices — untampered, unexpired, covering every
            // requested item on its exact trip, including passenger type.
            let quote = crate::quotes::verify(&state.quote_secret, token)
                .ok_or_else(|| ApiError::BadRequest("invalid or expired quote token".into()))?;
            let mut items = Vec::with_capacity(flat.len());
            for (trip_id, item) in &flat {
                let quantity = item.quantity.unwrap_or(1);
                let target = inventory
                    .resolve_target(*trip_id, &item.unit_code, &item.origin, &item.destination)
                    .await?
                    .ok_or_else(|| {
                        ApiError::NotFound(format!("trip {trip_id} with unit {:?}", item.unit_code))
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
                        q.trip_id == *trip_id
                            && q.unit_code == item.unit_code
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
                    trip_id: *trip_id,
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
            let priceable: Vec<crate::pricing::PriceableItem> = flat
                .iter()
                .map(|(trip_id, i)| crate::pricing::PriceableItem {
                    trip_id: *trip_id,
                    unit_code: i.unit_code.clone(),
                    origin: i.origin.clone(),
                    destination: i.destination.clone(),
                    quantity: i.quantity,
                    passenger_type: seat_passenger_type(i),
                })
                .collect();
            crate::pricing::price_items(&state, &priceable, req.promo_code.as_deref(), context)
                .await?
                .into_iter()
                .zip(&flat)
                .map(|(p, (_, i))| NewOrderItem {
                    trip_id: p.trip_id,
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

    match orders.create(&passengers, &items).await? {
        CreateOutcome::Created(record) => {
            // Attach ownership/contact (created in the same request, so
            // visible before the response ever leaves).
            sqlx::query("UPDATE orders SET customer_id = $2, guest_contact = $3 WHERE id = $1")
                .bind(record.order_id)
                .bind(customer_id)
                .bind(guest_contact)
                .execute(pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;

            // The itinerary hold has served its purpose; best-effort release.
            if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
                let _ = holds.release_itinerary(hold_id).await;
            }

            let response = CreateOrderResponse {
                retrieval_token: crate::identity::retrieval_token(
                    &state.quote_secret,
                    record.order_id,
                ),
                customer_id,
                record,
            };
            let body = serde_json::to_value(&response).map_err(|e| ApiError::Internal(e.into()))?;
            if let Some(key) = &idempotency_key {
                sqlx::query(
                    "INSERT INTO idempotency_keys (key, order_id, status_code, response)
                     VALUES ($1, $2, 201, $3) ON CONFLICT (key) DO NOTHING",
                )
                .bind(key)
                .bind(response.record.order_id)
                .bind(&body)
                .execute(pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
            }
            Ok((StatusCode::CREATED, Json(body)))
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

#[derive(Deserialize)]
pub struct GetOrderParams {
    /// Guest retrieval token issued at creation.
    #[serde(default)]
    token: Option<String>,
}

impl GetOrderParams {
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

/// Order reads are gated: retrieval token, owning customer, or an API
/// key. Public order enumeration was fine for a demo, not for PII.
pub(crate) async fn authorize_order_read(
    state: &AppState,
    headers: &HeaderMap,
    order_id: Uuid,
    token: Option<&str>,
) -> Result<(), ApiError> {
    if let Some(token) = token
        && crate::identity::verify_retrieval_token(&state.quote_secret, order_id, token)
    {
        return Ok(());
    }
    let pool = state.db.as_ref().expect("callers check db");
    // Owning customer?
    if let Some(subject) = crate::identity::bearer_subject(state, headers)
        && let Some(customer_id) = crate::identity::find_customer(pool, &subject)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?
    {
        let owns: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM orders WHERE id = $1 AND customer_id = $2")
                .bind(order_id)
                .bind(customer_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
        if owns.is_some() {
            return Ok(());
        }
    }
    // Server-to-server credential?
    if let Some(value) = headers
        .get("x-api-key")
        .or_else(|| headers.get("authorization"))
        && let Ok(raw) = value.to_str()
    {
        let key = raw.strip_prefix("Bearer ").unwrap_or(raw);
        if key.starts_with("llk_")
            && crate::auth::authenticate(pool, key)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?
                .is_some()
        {
            return Ok(());
        }
    }
    Err(ApiError::Unauthorized(
        "provide the order's retrieval token, the owning customer's bearer token, or an API key",
    ))
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
    Query(params): Query<GetOrderParams>,
    headers: HeaderMap,
) -> Result<Json<OrderDetails>, ApiError> {
    let orders = state.orders()?;
    authorize_order_read(&state, &headers, order_id, params.token.as_deref()).await?;
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
