//! Append-only event log + transactional outbox (ADR 0001).
//!
//! Events are appended inside the caller's transaction — one transaction,
//! one truth: an order transition, its inventory claims, and its events
//! commit or roll back together. A relay task drains the outbox to the
//! configured [`EventSink`]; Postgres enforces immutability with an
//! append-only trigger.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

/// An event as stored, with its global and per-stream position.
#[derive(Debug, Clone, Serialize)]
pub struct StoredEvent {
    pub sequence: i64,
    pub stream_id: Uuid,
    pub stream_seq: i32,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
}

/// Append one event (and its outbox row) inside `tx`. The stream sequence
/// is `max + 1`; callers must hold a lock that serialises appends per
/// stream (the order row lock does this for order streams).
pub async fn append(
    tx: &mut Transaction<'_, Postgres>,
    stream_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<i64, sqlx::Error> {
    let sequence: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO events (stream_id, stream_seq, event_type, payload)
        VALUES (
            $1,
            (SELECT coalesce(max(stream_seq), 0) + 1 FROM events WHERE stream_id = $1),
            $2,
            $3
        )
        RETURNING sequence
        "#,
    )
    .bind(stream_id)
    .bind(event_type)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query("INSERT INTO outbox (event_sequence) VALUES ($1)")
        .bind(sequence)
        .execute(&mut **tx)
        .await?;

    Ok(sequence)
}

/// All events of one stream in order — the input to replay.
pub async fn stream(pool: &PgPool, stream_id: Uuid) -> Result<Vec<StoredEvent>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT sequence, stream_id, stream_seq, event_type, payload, occurred_at
         FROM events WHERE stream_id = $1 ORDER BY stream_seq",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(StoredEvent {
                sequence: row.try_get("sequence")?,
                stream_id: row.try_get("stream_id")?,
                stream_seq: row.try_get("stream_seq")?,
                event_type: row.try_get("event_type")?,
                payload: row.try_get("payload")?,
                occurred_at: row.try_get("occurred_at")?,
            })
        })
        .collect()
}

/// Where outbox events go. Phase 3 ships [`TracingSink`]; Phase 5 adds a
/// webhook sink; Redpanda is a post-v1 feature-gated sink (ADR 0001).
pub trait EventSink: Send + Sync + 'static {
    fn deliver(
        &self,
        event: &StoredEvent,
    ) -> impl Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send;
}

/// Logs each event — the dev/default sink.
#[derive(Clone, Default)]
pub struct TracingSink;

impl EventSink for TracingSink {
    async fn deliver(
        &self,
        event: &StoredEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!(
            sequence = event.sequence,
            stream = %event.stream_id,
            stream_seq = event.stream_seq,
            event_type = %event.event_type,
            "event delivered"
        );
        Ok(())
    }
}

/// Drain one batch of undelivered outbox rows to `sink`. `FOR UPDATE SKIP
/// LOCKED` lets multiple relay instances coexist. Delivery is at-least-once:
/// a crash between deliver and commit re-delivers, so sinks must tolerate
/// duplicates (they carry the global sequence for dedup).
pub async fn relay_once<S: EventSink>(pool: &PgPool, sink: &S) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT o.id AS outbox_id, e.sequence, e.stream_id, e.stream_seq,
               e.event_type, e.payload, e.occurred_at
        FROM outbox o
        JOIN events e ON e.sequence = o.event_sequence
        WHERE o.delivered_at IS NULL
        ORDER BY o.id
        LIMIT 100
        FOR UPDATE OF o SKIP LOCKED
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut delivered = 0usize;
    for row in &rows {
        let event = StoredEvent {
            sequence: row.try_get("sequence")?,
            stream_id: row.try_get("stream_id")?,
            stream_seq: row.try_get("stream_seq")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
            occurred_at: row.try_get("occurred_at")?,
        };
        match sink.deliver(&event).await {
            Ok(()) => {
                let outbox_id: i64 = row.try_get("outbox_id")?;
                sqlx::query("UPDATE outbox SET delivered_at = now() WHERE id = $1")
                    .bind(outbox_id)
                    .execute(&mut *tx)
                    .await?;
                delivered += 1;
            }
            Err(err) => {
                // Stop the batch; undelivered rows retry next tick in order.
                tracing::warn!(error = %err, sequence = event.sequence, "event delivery failed");
                break;
            }
        }
    }
    tx.commit().await?;
    Ok(delivered)
}

/// Run the relay forever. Spawn as a background task.
pub async fn run_relay<S: EventSink>(pool: PgPool, sink: S, interval: std::time::Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match relay_once(&pool, &sink).await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(delivered = n, "outbox relay tick"),
            Err(err) => tracing::error!(error = %err, "outbox relay tick failed"),
        }
    }
}
