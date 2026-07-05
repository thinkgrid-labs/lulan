//! Webhook deliveries (Phase 6): the integration surface promised in
//! PRD §8/§9.
//!
//! Two-stage design keeps the outbox relay fast and deliveries durable:
//!
//! 1. [`WebhookSink`] (an [`EventSink`]) fans each outbox event out into
//!    `webhook_deliveries` rows — one per matching active endpoint. No
//!    network I/O on the relay path.
//! 2. [`deliver_due`] (run via [`run_delivery_worker`]) POSTs pending
//!    rows with exponential backoff until delivered or attempts run out.
//!    `FOR UPDATE SKIP LOCKED` lets workers scale horizontally.
//!
//! Deliveries are signed the Stripe way so receivers can authenticate
//! and reject replays:
//!
//! ```text
//! X-Lulan-Signature: t=<unix>,v1=hex(hmac_sha256(secret, "<t>.<body>"))
//! ```

use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::events::{EventSink, StoredEvent};

const MAX_ATTEMPTS: i32 = 8;
/// Base backoff; attempt n waits `BASE * 2^n` seconds (2s … ~4.5min).
const BASE_BACKOFF_SECS: i64 = 2;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compute the `X-Lulan-Signature` header value for a payload.
pub fn signature(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("t={timestamp},v1={}", hex(&mac.finalize().into_bytes()))
}

/// Verify a received `X-Lulan-Signature` header (receiver-side helper —
/// also what tests use).
pub fn verify_signature(secret: &str, header: &str, body: &str) -> bool {
    let mut timestamp = None;
    let mut sig = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => timestamp = v.parse::<i64>().ok(),
            Some(("v1", v)) => sig = Some(v.to_string()),
            _ => {}
        }
    }
    let (Some(timestamp), Some(sig)) = (timestamp, sig) else {
        return false;
    };
    let expected = signature(secret, timestamp, body);
    // Constant-time compare on the full header string.
    let provided = format!("t={timestamp},v1={sig}");
    expected.len() == provided.len()
        && expected
            .bytes()
            .zip(provided.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// EventSink that enqueues one durable delivery per matching endpoint.
#[derive(Clone)]
pub struct WebhookSink {
    pool: PgPool,
}

impl WebhookSink {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl EventSink for WebhookSink {
    async fn deliver(
        &self,
        event: &StoredEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Empty event_types means "everything".
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (endpoint_id, event_sequence)
            SELECT id, $1 FROM webhook_endpoints
            WHERE active AND (event_types = '{}' OR $2 = ANY(event_types))
            ON CONFLICT (endpoint_id, event_sequence) DO NOTHING
            "#,
        )
        .bind(event.sequence)
        .bind(&event.event_type)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct DeliveryStats {
    pub delivered: usize,
    pub retried: usize,
    pub exhausted: usize,
}

/// POST one batch of due deliveries. Returns what happened; callers loop.
pub async fn deliver_due(
    pool: &PgPool,
    client: &reqwest::Client,
) -> Result<DeliveryStats, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT d.id, d.attempts, w.url, w.secret,
               e.sequence, e.stream_id, e.stream_seq, e.event_type,
               e.payload, e.occurred_at
        FROM webhook_deliveries d
        JOIN webhook_endpoints w ON w.id = d.endpoint_id
        JOIN events e ON e.sequence = d.event_sequence
        WHERE d.status = 'pending' AND d.next_attempt_at <= now() AND w.active
        ORDER BY d.next_attempt_at
        LIMIT 20
        FOR UPDATE OF d SKIP LOCKED
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut stats = DeliveryStats {
        delivered: 0,
        retried: 0,
        exhausted: 0,
    };

    for row in &rows {
        let delivery_id: i64 = row.try_get("id")?;
        let attempts: i32 = row.try_get("attempts")?;
        let url: String = row.try_get("url")?;
        let secret: String = row.try_get("secret")?;

        let body = serde_json::json!({
            "sequence": row.try_get::<i64, _>("sequence")?,
            "stream_id": row.try_get::<Uuid, _>("stream_id")?,
            "stream_seq": row.try_get::<i32, _>("stream_seq")?,
            "event_type": row.try_get::<String, _>("event_type")?,
            "payload": row.try_get::<serde_json::Value, _>("payload")?,
            "occurred_at": row.try_get::<chrono::DateTime<Utc>, _>("occurred_at")?,
        })
        .to_string();

        let timestamp = Utc::now().timestamp();
        let result = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-lulan-signature", signature(&secret, timestamp, &body))
            .body(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        match result {
            Ok(response) if response.status().is_success() => {
                sqlx::query(
                    "UPDATE webhook_deliveries
                     SET status = 'delivered', attempts = attempts + 1, delivered_at = now()
                     WHERE id = $1",
                )
                .bind(delivery_id)
                .execute(&mut *tx)
                .await?;
                stats.delivered += 1;
            }
            outcome => {
                let error = match outcome {
                    Ok(response) => format!("HTTP {}", response.status()),
                    Err(err) => err.to_string(),
                };
                let next_attempt = attempts + 1;
                if next_attempt >= MAX_ATTEMPTS {
                    sqlx::query(
                        "UPDATE webhook_deliveries
                         SET status = 'failed', attempts = $2, last_error = $3
                         WHERE id = $1",
                    )
                    .bind(delivery_id)
                    .bind(next_attempt)
                    .bind(&error)
                    .execute(&mut *tx)
                    .await?;
                    tracing::error!(delivery_id, %url, %error, "webhook delivery exhausted");
                    stats.exhausted += 1;
                } else {
                    let backoff = BASE_BACKOFF_SECS << next_attempt.min(8);
                    sqlx::query(
                        "UPDATE webhook_deliveries
                         SET attempts = $2, last_error = $3,
                             next_attempt_at = now() + make_interval(secs => $4)
                         WHERE id = $1",
                    )
                    .bind(delivery_id)
                    .bind(next_attempt)
                    .bind(&error)
                    .bind(backoff as f64)
                    .execute(&mut *tx)
                    .await?;
                    stats.retried += 1;
                }
            }
        }
    }

    tx.commit().await?;
    Ok(stats)
}

/// Run the delivery worker forever. Spawn as a background task.
pub async fn run_delivery_worker(pool: PgPool, interval: std::time::Duration) {
    let client = reqwest::Client::new();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match deliver_due(&pool, &client).await {
            Ok(stats) if stats.delivered + stats.retried + stats.exhausted > 0 => {
                tracing::info!(
                    delivered = stats.delivered,
                    retried = stats.retried,
                    exhausted = stats.exhausted,
                    "webhook delivery pass"
                );
            }
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "webhook delivery pass failed"),
        }
    }
}
