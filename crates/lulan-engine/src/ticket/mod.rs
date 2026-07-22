//! Ticket issuance and boarding (Phase 5).
//!
//! On `Paid → Ticketed`, one **Ed25519-signed ticket per passenger-seat**
//! is issued. The wire format (shared contract with the
//! `lulan-validate` crate — keep the two in sync):
//!
//! ```text
//! token   = "LT1." base64url(payload) "." base64url(signature)
//! payload = CBOR map with short keys (see TicketClaims serde renames)
//! signature = Ed25519 over the raw payload bytes
//! ```
//!
//! Offline boarding: devices verify signatures locally (lulan-validate),
//! journal scans, and sync batches to `POST /v1/scans`. The idempotency
//! key (ticket, device, scanned_at) makes replayed uploads harmless;
//! duplicates across devices surface post-hoc — the honest threat model
//! (a signature cannot stop a cloned QR on two offline devices).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::{OrderEventType, OrderStatus, apply};
use crate::events;
use crate::inventory::StoreError;

/// Tickets stay valid this long after scheduled departure (late boarding,
/// delays).
const VALIDITY_AFTER_DEPARTURE_HOURS: i64 = 24;

/// The signed claims. Short serde names keep the CBOR payload compact for
/// low-error-correction QR codes. THIS IS A WIRE CONTRACT shared with
/// lulan-validate — never change field meanings, only add.
#[derive(Debug, Serialize, Deserialize)]
pub struct TicketClaims {
    /// Format version.
    pub v: u8,
    /// Ticket id (UUID bytes).
    #[serde(with = "serde_bytes_uuid")]
    pub tid: Uuid,
    /// Trip id (UUID bytes).
    #[serde(with = "serde_bytes_uuid")]
    pub trp: Uuid,
    /// Seat/unit code, e.g. `12A`.
    pub unt: String,
    /// Journey span [from, to).
    pub f: u8,
    pub t: u8,
    /// Passenger full name (checked against ID at boarding).
    pub pax: String,
    /// Fare class, if any.
    pub fc: Option<String>,
    /// Validity end, unix seconds.
    pub exp: i64,
    /// Signing key id.
    pub kid: String,
}

/// UUIDs as 16 raw bytes in CBOR (not 36-char strings).
mod serde_bytes_uuid {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(id: &Uuid, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(id.as_bytes())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Uuid, D::Error> {
        let bytes = serde_bytes::ByteBuf::deserialize(d)?;
        Uuid::from_slice(&bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    #[error("order is in state {0:?}; tickets require paid")]
    NotPaid(&'static str),
    #[error("order not found")]
    OrderNotFound,
    #[error("no active signing key")]
    NoSigningKey,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A loaded signing key.
#[derive(Clone)]
pub struct TicketSigner {
    pub kid: String,
    key: SigningKey,
}

impl TicketSigner {
    /// Build a signer from raw key material (KMS-provisioned keys, tests).
    pub fn from_seed(kid: impl Into<String>, seed: [u8; 32]) -> Self {
        Self {
            kid: kid.into(),
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// The base64url public half — what `GET /v1/ticket-keys` serves.
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes())
    }

    /// Load the active key, generating and persisting one on first boot.
    pub async fn load_or_create(pool: &PgPool) -> Result<Self, sqlx::Error> {
        if let Some(row) = sqlx::query(
            "SELECT kid, secret FROM ticket_keys WHERE active ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?
        {
            let secret: Vec<u8> = row.try_get("secret")?;
            let bytes: [u8; 32] = secret.try_into().expect("ticket_keys.secret is 32 bytes");
            return Ok(Self {
                kid: row.try_get("kid")?,
                key: SigningKey::from_bytes(&bytes),
            });
        }

        // 256 bits from two v4 UUIDs (~244 bits entropy) — fine for a
        // dev-generated key; production operators should rotate in a key
        // from their KMS.
        let mut seed = [0u8; 32];
        seed[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let key = SigningKey::from_bytes(&seed);
        let kid = format!("lulan-{}", &Uuid::new_v4().simple().to_string()[..8]);
        sqlx::query("INSERT INTO ticket_keys (kid, secret, public) VALUES ($1, $2, $3)")
            .bind(&kid)
            .bind(seed.as_slice())
            .bind(key.verifying_key().as_bytes().as_slice())
            .execute(pool)
            .await?;
        tracing::info!(%kid, "generated ticket signing key");
        Ok(Self { kid, key })
    }

    pub fn sign_token(&self, claims: &TicketClaims) -> String {
        let mut payload = Vec::new();
        ciborium::into_writer(claims, &mut payload).expect("claims serialize");
        let signature = self.key.sign(&payload);
        format!(
            "LT1.{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }
}

/// Public keys for validators (`GET /v1/ticket-keys`).
#[derive(Debug, Serialize)]
pub struct PublicKeyEntry {
    pub kid: String,
    pub alg: &'static str,
    /// base64url, 32 bytes.
    pub public_key: String,
}

pub async fn public_keys(pool: &PgPool) -> Result<Vec<PublicKeyEntry>, sqlx::Error> {
    let rows = sqlx::query("SELECT kid, public FROM ticket_keys ORDER BY created_at")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| {
            let public: Vec<u8> = row.try_get("public")?;
            Ok(PublicKeyEntry {
                kid: row.try_get("kid")?,
                alg: "Ed25519",
                public_key: URL_SAFE_NO_PAD.encode(public),
            })
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct IssuedTicket {
    pub ticket_id: Uuid,
    pub passenger_id: Uuid,
    pub passenger_name: String,
    pub unit_code: String,
    pub status: String,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct ScanOutcome {
    pub ticket_id: Uuid,
    /// `boarded` | `already_boarded` | `void` | `duplicate_scan` |
    /// `unknown_ticket`
    pub status: &'static str,
    pub order_status: Option<String>,
}

#[derive(Clone)]
pub struct TicketStore {
    pool: PgPool,
}

impl TicketStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Issue tickets for a paid order — one per seat item — and transition
    /// Paid → Ticketed, all in one transaction. Idempotent: an already
    /// Ticketed order returns its existing tickets.
    pub async fn issue_for_order(
        &self,
        order_id: Uuid,
        signer: &TicketSigner,
    ) -> Result<Vec<IssuedTicket>, TicketError> {
        let mut tx = self.pool.begin().await?;

        let Some(order) = sqlx::query("SELECT status FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Err(TicketError::OrderNotFound);
        };
        let status = OrderStatus::parse(order.get::<String, _>("status").as_str())
            .expect("orders.status CHECK guarantees a known value");

        if status == OrderStatus::Ticketed || status == OrderStatus::Boarded {
            drop(tx);
            return self.tickets_for_order(order_id).await;
        }
        if status != OrderStatus::Paid {
            return Err(TicketError::NotPaid(status.as_str()));
        }

        // One ticket per seat item, tied to its passenger; itineraries
        // yield one ticket per passenger PER LEG, each valid against its
        // own trip's departure.
        let seat_items = sqlx::query(
            "SELECT oi.trip_id, oi.unit_id, oi.unit_code, oi.from_index, oi.to_index,
                    p.id AS passenger_id, p.full_name,
                    cu.fare_class, t.departs_at
             FROM order_items oi
             JOIN passengers p ON p.id = oi.passenger_id
             JOIN capacity_units cu ON cu.id = oi.unit_id
             JOIN trips t ON t.id = oi.trip_id
             WHERE oi.order_id = $1 AND oi.kind = 'seat'
             ORDER BY t.departs_at, oi.unit_code",
        )
        .bind(order_id)
        .fetch_all(&mut *tx)
        .await?;

        let mut issued = Vec::with_capacity(seat_items.len());
        let mut issued_json = Vec::with_capacity(seat_items.len());
        for row in &seat_items {
            let ticket_id = Uuid::new_v4();
            let passenger_id: Uuid = row.try_get("passenger_id")?;
            let passenger_name: String = row.try_get("full_name")?;
            let unit_code: String = row.try_get("unit_code")?;
            let trip_id: Uuid = row.try_get("trip_id")?;
            let departs_at: DateTime<Utc> = row.try_get("departs_at")?;
            let exp = (departs_at + Duration::hours(VALIDITY_AFTER_DEPARTURE_HOURS)).timestamp();
            let claims = TicketClaims {
                v: 1,
                tid: ticket_id,
                trp: trip_id,
                unt: unit_code.clone(),
                f: row.try_get::<i16, _>("from_index")? as u8,
                t: row.try_get::<i16, _>("to_index")? as u8,
                pax: passenger_name.clone(),
                fc: row.try_get("fare_class")?,
                exp,
                kid: signer.kid.clone(),
            };
            let token = signer.sign_token(&claims);

            sqlx::query(
                "INSERT INTO tickets (id, order_id, passenger_id, trip_id, unit_id, token, kid)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(ticket_id)
            .bind(order_id)
            .bind(passenger_id)
            .bind(trip_id)
            .bind(row.try_get::<Uuid, _>("unit_id")?)
            .bind(&token)
            .bind(&signer.kid)
            .execute(&mut *tx)
            .await?;

            issued_json.push(json!({
                "ticket_id": ticket_id,
                "passenger_id": passenger_id,
                "trip_id": trip_id,
                "unit_code": unit_code,
            }));
            issued.push(IssuedTicket {
                ticket_id,
                passenger_id,
                passenger_name,
                unit_code,
                status: "issued".into(),
                token,
            });
        }

        // Paid → Ticketed (cargo-only orders ticket trivially with zero
        // passenger tickets — their receipt story is post-v1).
        let next = apply(Some(status), OrderEventType::TicketIssued)
            .expect("paid → ticketed is a legal transition");
        sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE id = $1")
            .bind(order_id)
            .bind(next.as_str())
            .execute(&mut *tx)
            .await?;
        events::append(
            &mut tx,
            order_id,
            OrderEventType::TicketIssued.as_str(),
            json!({ "tickets": issued_json, "kid": signer.kid }),
        )
        .await?;

        tx.commit().await?;
        Ok(issued)
    }

    pub async fn tickets_for_order(
        &self,
        order_id: Uuid,
    ) -> Result<Vec<IssuedTicket>, TicketError> {
        let rows = sqlx::query(
            "SELECT t.id, t.passenger_id, t.status, t.token, t.unit_id,
                    p.full_name, cu.code AS unit_code
             FROM tickets t
             JOIN passengers p ON p.id = t.passenger_id
             JOIN capacity_units cu ON cu.id = t.unit_id
             WHERE t.order_id = $1
             ORDER BY cu.code",
        )
        .bind(order_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(IssuedTicket {
                    ticket_id: row.try_get("id")?,
                    passenger_id: row.try_get("passenger_id")?,
                    passenger_name: row.try_get("full_name")?,
                    unit_code: row.try_get("unit_code")?,
                    status: row.try_get("status")?,
                    token: row.try_get("token")?,
                })
            })
            .collect()
    }

    /// Record one synced scan. Idempotent on (ticket, device, scanned_at).
    /// When the last ticket of an order boards, the order transitions
    /// Ticketed → Boarded with a passenger_boarded event.
    pub async fn record_scan(
        &self,
        ticket_id: Uuid,
        device_id: &str,
        scanned_at: DateTime<Utc>,
        result: &str,
    ) -> Result<ScanOutcome, TicketError> {
        let mut tx = self.pool.begin().await?;

        let Some(ticket) =
            sqlx::query("SELECT t.status, t.order_id FROM tickets t WHERE t.id = $1 FOR UPDATE")
                .bind(ticket_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(ScanOutcome {
                ticket_id,
                status: "unknown_ticket",
                order_status: None,
            });
        };
        let ticket_status: String = ticket.try_get("status")?;
        let order_id: Uuid = ticket.try_get("order_id")?;

        let inserted = sqlx::query(
            "INSERT INTO scan_events (ticket_id, device_id, scanned_at, result)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (ticket_id, device_id, scanned_at) DO NOTHING",
        )
        .bind(ticket_id)
        .bind(device_id)
        .bind(scanned_at)
        .bind(result)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if inserted == 0 {
            // Same device replaying its journal: harmless.
            tx.commit().await?;
            return Ok(ScanOutcome {
                ticket_id,
                status: "duplicate_scan",
                order_status: None,
            });
        }

        if ticket_status != "issued" {
            // Not boardable — but WHY matters at the gate, and these used
            // to be indistinguishable. A refunded ticket reported
            // "already_boarded", telling crew they were looking at a
            // duplicate scan when they were looking at a cancelled sale.
            // The journal row above is the post-hoc evidence either way.
            let status = match ticket_status.as_str() {
                "boarded" => "already_boarded",
                "void" => "void",
                _ => "not_boardable",
            };
            tx.commit().await?;
            return Ok(ScanOutcome {
                ticket_id,
                status,
                order_status: None,
            });
        }

        sqlx::query("UPDATE tickets SET status = 'boarded', boarded_at = $2 WHERE id = $1")
            .bind(ticket_id)
            .bind(scanned_at)
            .execute(&mut *tx)
            .await?;

        // Last passenger aboard → order becomes Boarded.
        let unboarded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM tickets WHERE order_id = $1 AND status = 'issued'",
        )
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
        let mut order_status = None;
        if unboarded == 0 {
            let current = OrderStatus::parse(
                sqlx::query_scalar::<_, String>(
                    "SELECT status FROM orders WHERE id = $1 FOR UPDATE",
                )
                .bind(order_id)
                .fetch_one(&mut *tx)
                .await?
                .as_str(),
            )
            .expect("known status");
            if let Ok(next) = apply(Some(current), OrderEventType::PassengerBoarded) {
                sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE id = $1")
                    .bind(order_id)
                    .bind(next.as_str())
                    .execute(&mut *tx)
                    .await?;
                events::append(
                    &mut tx,
                    order_id,
                    OrderEventType::PassengerBoarded.as_str(),
                    json!({ "all_aboard": true, "final_ticket": ticket_id }),
                )
                .await?;
                order_status = Some(next.as_str().to_string());
            }
        }

        tx.commit().await?;
        Ok(ScanOutcome {
            ticket_id,
            status: "boarded",
            order_status,
        })
    }
}
