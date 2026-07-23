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

pub mod sealing;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;
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

/// "GENERAL_ADMISSION" → "General Admission". Turns a pool's unit code
/// into a bearer-holder label for the gate display; leaves already-spaced
/// or mixed-case codes readable.
fn humanize(code: &str) -> String {
    code.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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
    Sealing(#[from] sealing::SealError),
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

    /// The key tickets are currently signed with, or `None` before one
    /// exists.
    ///
    /// Read at issue time rather than cached at boot, so a rotation takes
    /// effect immediately and on every replica. One indexed row read on a
    /// path that already opens a transaction is not worth the staleness a
    /// process-lifetime cache would introduce — a replica still signing
    /// with a retired key is precisely what rotation exists to stop.
    pub async fn active(pool: &PgPool) -> Result<Option<Self>, TicketError> {
        let Some(row) = sqlx::query(
            "SELECT kid, secret, encryption, nonce FROM ticket_keys
             WHERE active ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(Self::from_row(&row)?))
    }

    /// Decode one `ticket_keys` row, sealed or not.
    pub(crate) fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, TicketError> {
        let kid: String = row.try_get("kid")?;
        let secret: Vec<u8> = row.try_get("secret")?;
        let encryption: Option<String> = row.try_get("encryption")?;
        let nonce: Option<Vec<u8>> = row.try_get("nonce")?;
        let seed = sealing::unseal_row(&kid, encryption.as_deref(), nonce.as_deref(), &secret)?;
        Ok(Self {
            kid,
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Load one key by id — for inspecting a specific key, live or
    /// retired, without disturbing which one is active.
    pub async fn load(pool: &PgPool, kid: &str) -> Result<Option<Self>, TicketError> {
        let Some(row) =
            sqlx::query("SELECT kid, secret, encryption, nonce FROM ticket_keys WHERE kid = $1")
                .bind(kid)
                .fetch_optional(pool)
                .await?
        else {
            return Ok(None);
        };
        Ok(Some(Self::from_row(&row)?))
    }

    /// Load the active key, generating and persisting one on first boot.
    pub async fn load_or_create(pool: &PgPool) -> Result<Self, TicketError> {
        if let Some(signer) = Self::active(pool).await? {
            return Ok(signer);
        }
        Self::rotate(pool).await
    }

    /// Mint a new signing key and make it the active one.
    ///
    /// Previous keys are deactivated but NOT deleted: tickets already in
    /// passengers' wallets were signed with them and must keep verifying
    /// until they expire, so `GET /v1/ticket-keys` goes on publishing
    /// every public half. Rotation changes what gets signed next; it does
    /// not invalidate what was signed before. Retiring a key for real
    /// means rotating AND revoking the tickets it signed.
    pub async fn rotate(pool: &PgPool) -> Result<Self, TicketError> {
        // 32 bytes from the OS CSPRNG. The old construction stitched two
        // v4 UUIDs together, which is ~244 bits with six of them fixed by
        // the UUID version and variant fields — no way to derive a
        // signing key.
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let key = SigningKey::from_bytes(&seed);
        let kid = format!("lulan-{}", &Uuid::new_v4().simple().to_string()[..8]);

        // Sealed when the operator has configured a wrapping key; stored
        // as-is otherwise, which is the pre-existing behaviour.
        let sealed = sealing::KeyWrapper::from_env()?
            .map(|wrapper| wrapper.seal(&kid, &seed))
            .transpose()?;
        let (stored, scheme, nonce) = match &sealed {
            Some(s) => (
                s.ciphertext.as_slice(),
                Some(s.scheme),
                Some(s.nonce.as_slice()),
            ),
            None => (seed.as_slice(), None, None),
        };

        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE ticket_keys SET active = false WHERE active")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO ticket_keys (kid, secret, public, active, encryption, nonce)
             VALUES ($1, $2, $3, true, $4, $5)",
        )
        .bind(&kid)
        .bind(stored)
        .bind(key.verifying_key().as_bytes().as_slice())
        .bind(scheme)
        .bind(nonce)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        tracing::info!(%kid, sealed = sealed.is_some(), "ticket signing key rotated");
        Ok(Self { kid, key })
    }

    /// Seal any signing keys still stored in the clear.
    ///
    /// Run at boot so adopting encryption is "set the env var and
    /// restart" rather than a migration the operator has to script. A
    /// no-op when no wrapping key is configured, and idempotent: rows
    /// already sealed are skipped, so repeated boots and rolling restarts
    /// across replicas converge without coordination.
    ///
    /// Returns how many rows were sealed.
    pub async fn seal_stored_keys(pool: &PgPool) -> Result<usize, TicketError> {
        let Some(wrapper) = sealing::KeyWrapper::from_env()? else {
            return Ok(0);
        };
        let rows = sqlx::query("SELECT kid, secret FROM ticket_keys WHERE encryption IS NULL")
            .fetch_all(pool)
            .await?;

        let mut sealed_count = 0;
        for row in &rows {
            let kid: String = row.try_get("kid")?;
            let secret: Vec<u8> = row.try_get("secret")?;
            let seed: [u8; 32] = match secret.as_slice().try_into() {
                Ok(seed) => seed,
                Err(_) => {
                    tracing::error!(%kid, "stored ticket key is not 32 bytes; leaving it alone");
                    continue;
                }
            };
            let sealed = wrapper.seal(&kid, &seed)?;
            // Guarded on still being unsealed so two replicas booting at
            // once cannot seal the same row twice — the second would be
            // wrapping ciphertext.
            let updated = sqlx::query(
                "UPDATE ticket_keys SET secret = $2, encryption = $3, nonce = $4
                 WHERE kid = $1 AND encryption IS NULL",
            )
            .bind(&kid)
            .bind(&sealed.ciphertext)
            .bind(sealed.scheme)
            .bind(&sealed.nonce)
            .execute(pool)
            .await?
            .rows_affected();
            if updated == 1 {
                sealed_count += 1;
            }
        }
        if sealed_count > 0 {
            tracing::info!(count = sealed_count, "sealed ticket signing keys at rest");
        }
        Ok(sealed_count)
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
    pub async fn issue_for_order(&self, order_id: Uuid) -> Result<Vec<IssuedTicket>, TicketError> {
        let signer = TicketSigner::active(&self.pool)
            .await?
            .ok_or(TicketError::NoSigningKey)?;
        let signer = &signer;
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

        // Admission pools are bearer tickets: a pool line is order-level
        // with a quantity, and each admission is its own scannable QR.
        // Concert general admission and ferry foot passengers are the same
        // shape — capacity sold by the count, not the named seat. Each
        // admission gets a fresh bearer passenger so the (order, trip,
        // passenger, unit) uniqueness that stops a seat being double-issued
        // also stops a GA quantity being issued twice. Bulk pools (cargo
        // kilograms, vehicle-deck slots) are not admissions and issue no
        // per-unit tickets — the `admission` flag draws the line.
        let pool_items = sqlx::query(
            "SELECT oi.trip_id, oi.unit_id, oi.unit_code, oi.from_index, oi.to_index,
                    oi.quantity, cu.fare_class, t.departs_at
             FROM order_items oi
             JOIN capacity_units cu ON cu.id = oi.unit_id
             JOIN trips t ON t.id = oi.trip_id
             WHERE oi.order_id = $1 AND oi.kind = 'pool' AND cu.admission
             ORDER BY t.departs_at, oi.unit_code",
        )
        .bind(order_id)
        .fetch_all(&mut *tx)
        .await?;

        for row in &pool_items {
            let quantity: i32 = row.try_get("quantity")?;
            let unit_id: Uuid = row.try_get("unit_id")?;
            let unit_code: String = row.try_get("unit_code")?;
            let trip_id: Uuid = row.try_get("trip_id")?;
            let departs_at: DateTime<Utc> = row.try_get("departs_at")?;
            let fare_class: Option<String> = row.try_get("fare_class")?;
            let from_index = row.try_get::<i16, _>("from_index")? as u8;
            let to_index = row.try_get::<i16, _>("to_index")? as u8;
            let exp = (departs_at + Duration::hours(VALIDITY_AFTER_DEPARTURE_HOURS)).timestamp();
            // A bearer holder label, shown at the gate in the passenger
            // slot: "GENERAL_ADMISSION" reads as "General Admission".
            let label = humanize(&unit_code);

            for _ in 0..quantity.max(0) {
                let passenger_id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO passengers (id, order_id, full_name, passenger_type)
                     VALUES ($1, $2, $3, 'adult')",
                )
                .bind(passenger_id)
                .bind(order_id)
                .bind(&label)
                .execute(&mut *tx)
                .await?;

                let ticket_id = Uuid::new_v4();
                let claims = TicketClaims {
                    v: 1,
                    tid: ticket_id,
                    trp: trip_id,
                    unt: unit_code.clone(),
                    f: from_index,
                    t: to_index,
                    pax: label.clone(),
                    fc: fare_class.clone(),
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
                .bind(unit_id)
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
                    passenger_name: label.clone(),
                    unit_code: unit_code.clone(),
                    status: "issued".into(),
                    token,
                });
            }
        }

        // Paid → Ticketed. An order with neither seats nor pool admissions
        // (a pure ancillary/cargo receipt) still transitions, with zero
        // tickets.
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

    /// Ticket ids a gate must refuse even though their signature is
    /// perfectly valid — refunded orders, cancelled trips, voided seats.
    ///
    /// A signature proves a ticket was issued; it cannot prove it is still
    /// good, because the cancellation happened after signing. Offline
    /// devices therefore cache this alongside the key set. Scoped to trips
    /// departing inside `horizon_hours` so the list a device carries stays
    /// small and current rather than growing forever.
    ///
    /// This is revocation *detection*, bounded by how recently a device
    /// synced — the same honest limit as clone detection. A device that
    /// has never synced cannot know.
    pub async fn revoked_tickets(
        &self,
        trip_id: Option<Uuid>,
        horizon_hours: i64,
    ) -> Result<Vec<Uuid>, TicketError> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT t.id
             FROM tickets t
             JOIN trips tr ON tr.id = t.trip_id
             WHERE t.status = 'void'
               AND (
                 -- Asked about one departure: answer completely, whenever
                 -- it leaves. Truncating this by a time window would hand
                 -- a gate an empty list for a trip departing next week and
                 -- let a refunded passenger board.
                 ($1::uuid IS NOT NULL AND t.trip_id = $1)
                 -- Asked about everything: bound it, or the list grows
                 -- without limit and devices carry history they will never
                 -- scan.
                 OR ($1::uuid IS NULL
                     AND tr.departs_at BETWEEN now() - interval '24 hours'
                                           AND now() + make_interval(hours => $2))
               )
             ORDER BY t.id",
        )
        .bind(trip_id)
        .bind(horizon_hours as i32)
        .fetch_all(&self.pool)
        .await?)
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

#[cfg(test)]
mod tests {
    use super::humanize;

    #[test]
    fn humanize_makes_pool_codes_readable() {
        assert_eq!(humanize("GENERAL_ADMISSION"), "General Admission");
        assert_eq!(humanize("FOOT_PASSENGER"), "Foot Passenger");
        assert_eq!(humanize("vip"), "Vip");
        assert_eq!(humanize("Lawn  Access"), "Lawn Access");
        assert_eq!(humanize(""), "");
    }
}
