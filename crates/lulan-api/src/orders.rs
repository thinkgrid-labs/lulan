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
    CreateOutcome, ItemValidation, NewOrderAncillary, NewOrderItem, NewPassenger, OrderRecord,
    TransitionOutcome,
};
use lulan_engine::payments::{FakeProvider, PaymentProvider};
use lulan_engine::ticket::TicketStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_PASSENGERS: usize = 20;

/// Itinerary shape (`journeys`) or single-trip shape (`trip_id`+`items`).
///
/// `Serialize` is not for output: it produces the canonical form hashed as
/// the `Idempotency-Key` request fingerprint, so field order is fixed here
/// rather than by whatever JSON the client happened to send.
#[derive(Deserialize, Serialize)]
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
    /// Add-ons from GET /v1/ancillaries (must match the quote token when
    /// one is presented).
    #[serde(default)]
    ancillaries: Vec<crate::ancillaries::AncillaryLine>,
}

#[derive(Deserialize, Serialize)]
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

#[derive(Deserialize, Serialize)]
pub struct PassengerRequest {
    full_name: String,
    #[serde(rename = "type")]
    passenger_type: String,
    #[serde(default)]
    birthdate: Option<NaiveDate>,
}

#[derive(Deserialize, Serialize)]
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
    // Fingerprint the request before it is destructured, so a retry under
    // the same key is checked against what was actually asked for.
    let idempotency_key = crate::idempotency::key_from_headers(&headers);
    let request_hash = crate::idempotency::request_hash(&req);

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

    // Identity: a customer bearer token, or guest checkout with contact.
    // Resolved BEFORE the idempotency key, which is scoped to it — a key
    // means "this caller's request N", never "request N".
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
    let idempotency_scope = crate::idempotency::scope(customer_id, guest_contact);

    // A plain retry of a finished booking answers here, before pricing.
    if let Some((status, response)) = crate::idempotency::replay_if_completed(
        pool,
        idempotency_key.as_deref(),
        &idempotency_scope,
        &request_hash,
    )
    .await?
    {
        return Ok((status, Json(response)));
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

    // A presented hold must be alive — the deterministic "session expired,
    // re-select your seats" moment clients rely on. Deliberately checked
    // BEFORE pricing so the customer isn't shown a price they can't get.
    // Redis being down never blocks a sale (ADR 0002): verification errors
    // fall through to the authoritative claims. Orders without a hold_id
    // skip all of this.
    if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
        match holds.itinerary_members(hold_id).await {
            Ok(Some(members)) => {
                let covers_trip = |trip_id: &Uuid| members.iter().any(|m| m.trip_id == *trip_id);
                if !flat.iter().all(|(trip_id, _)| covers_trip(trip_id)) {
                    return Err(ApiError::Conflict(
                        "hold does not cover this itinerary's trips".into(),
                    ));
                }
            }
            Ok(None) => {
                return Err(ApiError::Conflict(
                    "hold expired — the seats were released; re-select and hold again (or retry without hold_id)"
                        .into(),
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, %hold_id, "hold verification unavailable — proceeding to claims");
            }
        }
    }
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

    let mut quote_ancillaries: Option<Vec<crate::quotes::QuoteTokenAncillary>> = None;
    // The unit the order is denominated in: whatever priced it. A quote
    // token carries the currency it locked; live pricing reports the active
    // ruleset's. Never assumed — an operator selling in anything but the
    // engine's old hard-coded PHP used to get a quote in their currency and
    // an order row in pesos.
    let currency: String;
    let items: Vec<NewOrderItem> = match &req.quote_token {
        Some(token) => {
            // Honour quoted prices — untampered, unexpired, covering every
            // requested item on its exact trip, including passenger type.
            let quote = crate::quotes::verify(&state.quote_secret, token)
                .ok_or_else(|| ApiError::BadRequest("invalid or expired quote token".into()))?;
            quote_ancillaries = Some(quote.ancillaries);
            currency = quote.currency;
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
            let priced =
                crate::pricing::price_items(&state, &priceable, req.promo_code.as_deref(), context)
                    .await?;
            // One active ruleset prices the whole itinerary, so every line
            // shares its currency; assert rather than silently pick one.
            currency = priced[0].quote.currency.clone();
            if let Some(odd) = priced.iter().find(|p| p.quote.currency != currency) {
                return Err(ApiError::Internal(anyhow::anyhow!(
                    "fare rules priced one itinerary in two currencies ({currency} and {})",
                    odd.quote.currency
                )));
            }
            priced
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

    // Ancillary lines: validated against the catalog; with a quote token
    // they must match the quoted lines verbatim (same code/leg/passenger/
    // quantity) at the locked totals.
    let itinerary_trips: Vec<Uuid> = journeys.iter().map(|(trip_id, _)| *trip_id).collect();
    let priced_ancillaries =
        crate::ancillaries::price_lines(pool, &req.ancillaries, &itinerary_trips).await?;
    let ancillary_lines: Vec<NewOrderAncillary> = match &quote_ancillaries {
        Some(quoted) => {
            let mut lines = Vec::with_capacity(priced_ancillaries.len());
            if quoted.len() != priced_ancillaries.len() {
                return Err(ApiError::BadRequest(
                    "ancillaries differ from the quote — re-quote or match them exactly".into(),
                ));
            }
            for priced in &priced_ancillaries {
                let locked = quoted
                    .iter()
                    .find(|q| q.line == priced.line)
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!(
                            "ancillary {} is not covered by the quote",
                            priced.line.code
                        ))
                    })?;
                lines.push(NewOrderAncillary {
                    ancillary_id: priced.ancillary.id,
                    code: priced.ancillary.code.clone(),
                    name: priced.ancillary.name.clone(),
                    trip_id: priced.line.trip_id,
                    passenger_index: priced.line.passenger,
                    quantity: priced.line.quantity,
                    total_minor: locked.total_minor,
                });
            }
            lines
        }
        None => priced_ancillaries
            .iter()
            .map(|priced| NewOrderAncillary {
                ancillary_id: priced.ancillary.id,
                code: priced.ancillary.code.clone(),
                name: priced.ancillary.name.clone(),
                trip_id: priced.line.trip_id,
                passenger_index: priced.line.passenger,
                quantity: priced.line.quantity,
                total_minor: priced.total_minor,
            })
            .collect(),
    };

    // Claim the idempotency key immediately before the write: everything
    // from here to complete/release is the window a concurrent retry sees
    // as in-flight, so it stays as short as possible.
    let reservation =
        match crate::idempotency::reserve(pool, idempotency_key, idempotency_scope, &request_hash)
            .await?
        {
            crate::idempotency::Reserved::Replay(status, response) => {
                return Ok((status, Json(response)));
            }
            crate::idempotency::Reserved::Held(reservation) => Some(reservation),
            crate::idempotency::Reserved::Disabled => None,
        };

    let outcome = orders
        .create(lulan_engine::orders::NewOrder {
            passengers: &passengers,
            items: &items,
            ancillaries: &ancillary_lines,
            currency: &currency,
            customer_id,
            guest_contact,
        })
        .await;

    // Nothing was booked on any failing path — give the key back so the
    // caller can retry with it (after choosing another seat, say).
    let record = match outcome {
        Ok(CreateOutcome::Created(record)) => record,
        other => {
            if let Some(reservation) = &reservation {
                crate::idempotency::release(pool, reservation).await;
            }
            return Err(match other {
                Err(err) => err.into(),
                Ok(CreateOutcome::Conflict { unit_code }) => ApiError::Conflict(format!(
                    "{unit_code} is no longer available for the requested span"
                )),
                Ok(CreateOutcome::NotFound { what }) => {
                    ApiError::NotFound(format!("{what} not found"))
                }
                Ok(CreateOutcome::Invalid(validation)) => ApiError::BadRequest(match validation {
                    ItemValidation::SeatNeedsPassenger { unit_code } => {
                        format!("seat {unit_code} needs a passenger index")
                    }
                    ItemValidation::PassengerIndexOutOfRange { unit_code, index } => {
                        format!(
                            "seat {unit_code} references passenger {index}, which does not exist"
                        )
                    }
                    ItemValidation::PoolWithPassenger { unit_code } => {
                        format!("pool item {unit_code} cannot reference a passenger")
                    }
                    ItemValidation::NoPassengers => "orders need at least one passenger".into(),
                }),
                Ok(CreateOutcome::Created(_)) => unreachable!("matched above"),
            });
        }
    };

    // The itinerary hold has served its purpose; best-effort release.
    if let (Some(hold_id), Ok(holds)) = (req.hold_id, state.holds()) {
        let _ = holds.release_itinerary(hold_id).await;
    }

    let order_id = record.order_id;
    let response = CreateOrderResponse {
        retrieval_token: crate::identity::retrieval_token(&state.quote_secret, order_id),
        customer_id,
        record,
    };
    let body = serde_json::to_value(&response).map_err(|e| ApiError::Internal(e.into()))?;
    if let Some(reservation) = &reservation {
        crate::idempotency::complete(pool, reservation, order_id, StatusCode::CREATED, &body)
            .await?;
    }
    Ok((StatusCode::CREATED, Json(body)))
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

/// Access to one order — reading it, paying it, cancelling it — is gated
/// on one of: its retrieval token, the owning customer's bearer token, or
/// an API key. Knowing the order id is not a credential: it appears in
/// logs, URLs, and confirmation emails.
pub(crate) async fn authorize_order_access(
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
    authorize_order_access(&state, &headers, order_id, params.token.as_deref()).await?;
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
/// configured provider (FakeProvider until a real adapter lands). Gated
/// like the order itself: the intent id it returns is what captures the
/// payment, so it must not be mintable by anyone holding the order id.
pub async fn request_payment(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
    Query(params): Query<GetOrderParams>,
    headers: HeaderMap,
) -> Result<Json<PaymentResponse>, ApiError> {
    let orders = state.orders()?;
    authorize_order_access(&state, &headers, order_id, params.token()).await?;
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
///
/// Requires an `integration` or `operator_admin` key. This endpoint turns
/// an order into a boarding pass, so it needs the same trust as the
/// counter-payment path it stands in for: a real provider authenticates
/// with a signed callback, and until such an adapter exists that trust is
/// carried by the API key. Anonymous access here is free travel.
pub async fn fake_webhook(
    State(state): State<AppState>,
    _auth: crate::auth::IntegrationAuth,
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
/// Gated like the order itself: cancelling destroys a booking and frees
/// its seats, so the order id alone must not authorise it.
pub async fn cancel(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
    Query(params): Query<GetOrderParams>,
    headers: HeaderMap,
) -> Result<Json<CancelResponse>, ApiError> {
    let orders = state.orders()?;
    authorize_order_access(&state, &headers, order_id, params.token()).await?;
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
