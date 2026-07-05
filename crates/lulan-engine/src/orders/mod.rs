//! Order engine: creation, lifecycle transitions, expiry, and replay.
//!
//! Invariant (ADR 0002 + plan §Phase 3): an order's inventory claims and
//! its events commit in the SAME transaction as the order row. There is no
//! moment where an order exists without its claims, or a claim without its
//! audit trail. Transitions take a `FOR UPDATE` lock on the order row,
//! which also serialises event appends per stream.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::domain::{OrderEventType, OrderStatus, SegmentSpan, apply};
use crate::events;
use crate::inventory::{
    InventoryStore, StoreError, claim_pool_exec, claim_seat_exec, release_pool_exec,
    release_seat_exec,
};

pub const CURRENCY: &str = "PHP";
/// How long claims stay provisional awaiting payment.
pub const ORDER_TTL_MINUTES: i64 = 15;

#[derive(Debug, Clone)]
pub struct NewOrderItem {
    pub unit_code: String,
    pub origin: String,
    pub destination: String,
    pub quantity: i32,
    /// Priced by the caller (live engine quote or verified quote token) —
    /// the order engine records money, it never computes it.
    pub price_minor: i64,
}

#[derive(Debug, Serialize)]
pub struct OrderItem {
    pub unit_code: String,
    pub kind: String,
    pub from_index: u8,
    pub to_index: u8,
    pub quantity: i32,
    pub price_minor: i64,
}

#[derive(Debug, Serialize)]
pub struct OrderRecord {
    pub order_id: Uuid,
    pub trip_id: Uuid,
    pub passenger_name: String,
    pub status: OrderStatus,
    pub total_minor: i64,
    pub currency: String,
    pub payment_intent_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub items: Vec<OrderItem>,
}

/// Result of creating an order.
#[derive(Debug)]
pub enum CreateOutcome {
    Created(OrderRecord),
    /// A claim lost the race; nothing was written.
    Conflict {
        unit_code: String,
    },
    /// Trip or a unit doesn't exist.
    NotFound {
        what: String,
    },
}

/// Result of a lifecycle transition.
#[derive(Debug, PartialEq, Eq)]
pub enum TransitionOutcome {
    Applied(OrderStatus),
    /// The event is illegal in the current state. Deliberately not an
    /// error: duplicate/out-of-order webhooks land here and must be
    /// acknowledged idempotently.
    NoOp(OrderStatus),
    NotFound,
}

#[derive(Clone)]
pub struct OrderStore {
    pool: PgPool,
    inventory: InventoryStore,
}

impl OrderStore {
    pub fn new(pool: PgPool) -> Self {
        let inventory = InventoryStore::new(pool.clone());
        Self { pool, inventory }
    }

    /// Create an order: claims + order row + OrderCreated/InventoryLocked
    /// events, atomically. Any claim conflict rolls back everything.
    pub async fn create(
        &self,
        trip_id: Uuid,
        passenger_name: &str,
        items: &[NewOrderItem],
    ) -> Result<CreateOutcome, StoreError> {
        if items.is_empty() {
            return Ok(CreateOutcome::NotFound {
                what: "items (order must contain at least one)".into(),
            });
        }

        // Resolve everything read-only before opening the write transaction.
        let mut targets = Vec::with_capacity(items.len());
        for item in items {
            match self
                .inventory
                .resolve_target(trip_id, &item.unit_code, &item.origin, &item.destination)
                .await?
            {
                Some(target) => targets.push(target),
                None => {
                    return Ok(CreateOutcome::NotFound {
                        what: format!("trip {trip_id} with unit {:?}", item.unit_code),
                    });
                }
            }
        }

        let order_id = Uuid::new_v4();
        let expires_at = Utc::now() + chrono::Duration::minutes(ORDER_TTL_MINUTES);
        let total_minor: i64 = items.iter().map(|i| i.price_minor).sum();

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO orders (id, trip_id, passenger_name, status, total_minor, currency, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(order_id)
        .bind(trip_id)
        .bind(passenger_name)
        .bind(OrderStatus::Locked.as_str())
        .bind(total_minor)
        .bind(CURRENCY)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;

        let mut recorded_items = Vec::with_capacity(items.len());
        for (item, target) in items.iter().zip(&targets) {
            let rows = match target.kind.as_str() {
                "seat" => claim_seat_exec(&mut *tx, trip_id, target.unit_id, target.span).await?,
                _ => {
                    if item.quantity <= 0 {
                        0
                    } else {
                        claim_pool_exec(
                            &mut *tx,
                            trip_id,
                            target.unit_id,
                            target.span,
                            item.quantity,
                        )
                        .await?
                    }
                }
            };
            if rows != 1 {
                tx.rollback().await?;
                return Ok(CreateOutcome::Conflict {
                    unit_code: item.unit_code.clone(),
                });
            }
            sqlx::query(
                "INSERT INTO order_items (order_id, unit_id, unit_code, kind, from_index, to_index, quantity, price_minor)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(order_id)
            .bind(target.unit_id)
            .bind(&item.unit_code)
            .bind(&target.kind)
            .bind(i16::from(target.span.from_index()))
            .bind(i16::from(target.span.to_index()))
            .bind(item.quantity.max(1))
            .bind(item.price_minor)
            .execute(&mut *tx)
            .await?;
            recorded_items.push(OrderItem {
                unit_code: item.unit_code.clone(),
                kind: target.kind.clone(),
                from_index: target.span.from_index(),
                to_index: target.span.to_index(),
                quantity: item.quantity.max(1),
                price_minor: item.price_minor,
            });
        }

        let items_json: Vec<_> = recorded_items
            .iter()
            .map(|i| {
                json!({
                    "unit_code": i.unit_code, "kind": i.kind,
                    "from_index": i.from_index, "to_index": i.to_index,
                    "quantity": i.quantity, "price_minor": i.price_minor,
                })
            })
            .collect();
        events::append(
            &mut tx,
            order_id,
            OrderEventType::OrderCreated.as_str(),
            json!({
                "trip_id": trip_id,
                "passenger_name": passenger_name,
                "total_minor": total_minor,
                "currency": CURRENCY,
                "items": items_json,
            }),
        )
        .await?;
        events::append(
            &mut tx,
            order_id,
            OrderEventType::InventoryLocked.as_str(),
            json!({ "items": items_json, "expires_at": expires_at }),
        )
        .await?;

        tx.commit().await?;

        Ok(CreateOutcome::Created(OrderRecord {
            order_id,
            trip_id,
            passenger_name: passenger_name.to_string(),
            status: OrderStatus::Locked,
            total_minor,
            currency: CURRENCY.to_string(),
            payment_intent_id: None,
            expires_at: Some(expires_at),
            items: recorded_items,
        }))
    }

    /// Locked → PendingPayment, recording the provider intent id.
    pub async fn request_payment(
        &self,
        order_id: Uuid,
        payment_intent_id: &str,
    ) -> Result<TransitionOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .apply_event_locked(
                &mut tx,
                order_id,
                OrderEventType::PaymentRequested,
                json!({ "payment_intent_id": payment_intent_id }),
            )
            .await?;
        if let TransitionOutcome::Applied(_) = outcome {
            sqlx::query(
                "UPDATE orders SET payment_intent_id = $2, updated_at = now() WHERE id = $1",
            )
            .bind(order_id)
            .bind(payment_intent_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Reconcile a provider webhook by intent id. Idempotent: duplicates and
    /// out-of-order deliveries resolve to NoOp with the current state.
    pub async fn apply_payment_result(
        &self,
        payment_intent_id: &str,
        succeeded: bool,
    ) -> Result<TransitionOutcome, StoreError> {
        let Some(order_id) =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM orders WHERE payment_intent_id = $1")
                .bind(payment_intent_id)
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(TransitionOutcome::NotFound);
        };

        let event = if succeeded {
            OrderEventType::PaymentCaptured
        } else {
            OrderEventType::PaymentFailed
        };

        let mut tx = self.pool.begin().await?;
        let outcome = self
            .apply_event_locked(
                &mut tx,
                order_id,
                event,
                json!({ "payment_intent_id": payment_intent_id }),
            )
            .await?;
        if let TransitionOutcome::Applied(OrderStatus::Paid) = outcome {
            // Paid claims are permanent: stop the expiry clock.
            sqlx::query("UPDATE orders SET expires_at = NULL, updated_at = now() WHERE id = $1")
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Cancel a not-yet-paid order, releasing its claims atomically.
    pub async fn cancel(&self, order_id: Uuid) -> Result<TransitionOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        let outcome = self
            .apply_event_locked(&mut tx, order_id, OrderEventType::OrderCancelled, json!({}))
            .await?;
        if let TransitionOutcome::Applied(_) = outcome {
            release_order_items(&mut tx, order_id).await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Expire every overdue order, releasing claims. Returns count expired.
    /// Called by the background sweeper; safe to run concurrently
    /// (`FOR UPDATE SKIP LOCKED`).
    pub async fn expire_due(&self) -> Result<usize, StoreError> {
        let due: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM orders
             WHERE status IN ('locked', 'pending_payment') AND expires_at < now()",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut expired = 0;
        for order_id in due {
            let mut tx = self.pool.begin().await?;
            let outcome = self
                .apply_event_locked(&mut tx, order_id, OrderEventType::OrderExpired, json!({}))
                .await?;
            if let TransitionOutcome::Applied(_) = outcome {
                release_order_items(&mut tx, order_id).await?;
                expired += 1;
            }
            tx.commit().await?;
        }
        Ok(expired)
    }

    /// One lifecycle step under the order row lock: check legality via the
    /// pure state machine, update the read model, append exactly one event.
    async fn apply_event_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        order_id: Uuid,
        event: OrderEventType,
        payload: serde_json::Value,
    ) -> Result<TransitionOutcome, StoreError> {
        let Some(row) = sqlx::query("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut **tx)
            .await?
        else {
            return Ok(TransitionOutcome::NotFound);
        };
        let current = OrderStatus::parse(row.get::<String, _>(0).as_str())
            .expect("orders.status CHECK constraint guarantees a known value");

        let Ok(next) = apply(Some(current), event) else {
            return Ok(TransitionOutcome::NoOp(current));
        };

        sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE id = $1")
            .bind(order_id)
            .bind(next.as_str())
            .execute(&mut **tx)
            .await?;
        events::append(tx, order_id, event.as_str(), payload).await?;
        Ok(TransitionOutcome::Applied(next))
    }

    /// Fetch the read model for one order.
    pub async fn get(&self, order_id: Uuid) -> Result<Option<OrderRecord>, StoreError> {
        let Some(row) = sqlx::query(
            "SELECT trip_id, passenger_name, status, total_minor, currency,
                    payment_intent_id, expires_at
             FROM orders WHERE id = $1",
        )
        .bind(order_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        let item_rows = sqlx::query(
            "SELECT unit_code, kind, from_index, to_index, quantity, price_minor
             FROM order_items WHERE order_id = $1 ORDER BY unit_code",
        )
        .bind(order_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(Some(OrderRecord {
            order_id,
            trip_id: row.try_get("trip_id")?,
            passenger_name: row.try_get("passenger_name")?,
            status: OrderStatus::parse(row.get::<String, _>("status").as_str())
                .expect("orders.status CHECK constraint guarantees a known value"),
            total_minor: row.try_get("total_minor")?,
            currency: row.try_get("currency")?,
            payment_intent_id: row.try_get("payment_intent_id")?,
            expires_at: row.try_get("expires_at")?,
            items: item_rows
                .into_iter()
                .map(|r| {
                    Ok(OrderItem {
                        unit_code: r.try_get("unit_code")?,
                        kind: r.try_get("kind")?,
                        from_index: r.try_get::<i16, _>("from_index")? as u8,
                        to_index: r.try_get::<i16, _>("to_index")? as u8,
                        quantity: r.try_get("quantity")?,
                        price_minor: r.try_get("price_minor")?,
                    })
                })
                .collect::<Result<Vec<_>, sqlx::Error>>()?,
        }))
    }

    /// Fold the event stream through the pure state machine — the Phase 3
    /// exit criterion is that this always equals the read model's status.
    pub async fn replay_status(&self, order_id: Uuid) -> Result<Option<OrderStatus>, StoreError> {
        let stream = events::stream(&self.pool, order_id).await?;
        if stream.is_empty() {
            return Ok(None);
        }
        let mut state: Option<OrderStatus> = None;
        for event in &stream {
            let event_type = OrderEventType::parse(&event.event_type)
                .unwrap_or_else(|| panic!("unknown event type {:?} in stream", event.event_type));
            state = Some(apply(state, event_type).unwrap_or_else(|e| {
                panic!(
                    "stored stream must replay cleanly, got {e} at seq {}",
                    event.stream_seq
                )
            }));
        }
        Ok(state)
    }
}

/// Release every claim held by an order (cancel/expire paths). Runs inside
/// the caller's transaction, guarded exactly like claims.
async fn release_order_items(
    tx: &mut Transaction<'_, Postgres>,
    order_id: Uuid,
) -> Result<(), StoreError> {
    let items = sqlx::query(
        "SELECT oi.unit_id, oi.kind, oi.from_index, oi.to_index, oi.quantity, o.trip_id
         FROM order_items oi JOIN orders o ON o.id = oi.order_id
         WHERE oi.order_id = $1",
    )
    .bind(order_id)
    .fetch_all(&mut **tx)
    .await?;

    for row in items {
        let unit_id: Uuid = row.try_get("unit_id")?;
        let trip_id: Uuid = row.try_get("trip_id")?;
        let kind: String = row.try_get("kind")?;
        let from = row.try_get::<i16, _>("from_index")? as u8;
        let to = row.try_get::<i16, _>("to_index")? as u8;
        let span = SegmentSpan::new(from, to)?;
        let released = match kind.as_str() {
            "seat" => release_seat_exec(&mut **tx, trip_id, unit_id, span).await?,
            _ => {
                let qty: i32 = row.try_get("quantity")?;
                release_pool_exec(&mut **tx, trip_id, unit_id, span, qty).await?
            }
        };
        if released != 1 {
            // A release failing means the ledger is inconsistent — surface
            // loudly rather than silently absorbing it.
            tracing::error!(%order_id, %unit_id, "claim release failed during cancel/expire");
        }
    }
    Ok(())
}
