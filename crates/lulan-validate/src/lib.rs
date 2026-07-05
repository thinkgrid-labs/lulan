//! Offline ticket verification (Phase 5). MIT-licensed and deliberately
//! dependency-light so proprietary crew and kiosk apps can embed it; compiles
//! to wasm32 for `@lulan/validate` (browser + React Native).
//!
//! Pure by construction: no clock (callers pass `now_unix`), no network,
//! no storage. Ground staff cache the operator's public keys from
//! `GET /v1/ticket-keys` while online; from then on validation is fully
//! offline.
//!
//! Wire contract (shared with `lulan-engine::ticket` — keep in sync):
//! `token = "LT1." base64url(cbor(claims)) "." base64url(ed25519_sig)`,
//! signature over the raw CBOR payload bytes.
//!
//! What a signature does and doesn't prove (the honest threat model): a
//! valid signature proves the ticket was issued by the operator and not
//! altered. It cannot prove the QR wasn't *cloned* — same-device re-scans
//! are rejected by the device's local seen-set, and cross-device
//! duplicates are detected server-side when scan journals sync.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A cached public key, as served by `GET /v1/ticket-keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub kid: String,
    /// base64url-encoded 32-byte Ed25519 public key.
    pub public_key: String,
}

/// The signed claims (mirror of `lulan-engine::ticket::TicketClaims`).
#[derive(Debug, Serialize, Deserialize)]
struct TicketClaims {
    v: u8,
    #[serde(with = "serde_bytes_uuid")]
    tid: Uuid,
    #[serde(with = "serde_bytes_uuid")]
    trp: Uuid,
    unt: String,
    f: u8,
    t: u8,
    pax: String,
    fc: Option<String>,
    exp: i64,
    kid: String,
}

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

/// A successfully verified ticket, ready to display to ground staff.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerifiedTicket {
    pub ticket_id: Uuid,
    pub trip_id: Uuid,
    pub unit_code: String,
    pub from_index: u8,
    pub to_index: u8,
    pub passenger_name: String,
    pub fare_class: Option<String>,
    pub expires_at_unix: i64,
    pub kid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ValidationError {
    #[error("token is malformed")]
    Malformed,
    #[error("unsupported ticket version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown signing key {kid:?} — refresh the cached key set")]
    UnknownKey { kid: String },
    #[error("signature verification failed")]
    BadSignature,
    #[error("ticket expired at {expired_at_unix}")]
    Expired { expired_at_unix: i64 },
    #[error("ticket is for a different trip")]
    WrongTrip,
}

/// Verify a scanned token against cached public keys. `expected_trip`
/// pins validation to the trip being boarded (recommended); pass `None`
/// for a generic inspection scan.
pub fn verify_ticket(
    token: &str,
    keys: &[KeyEntry],
    now_unix: i64,
    expected_trip: Option<Uuid>,
) -> Result<VerifiedTicket, ValidationError> {
    let rest = token
        .strip_prefix("LT1.")
        .ok_or(ValidationError::Malformed)?;
    let (payload_b64, sig_b64) = rest.split_once('.').ok_or(ValidationError::Malformed)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| ValidationError::Malformed)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| ValidationError::Malformed)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| ValidationError::Malformed)?;

    let claims: TicketClaims =
        ciborium::from_reader(payload.as_slice()).map_err(|_| ValidationError::Malformed)?;
    if claims.v != 1 {
        return Err(ValidationError::UnsupportedVersion(claims.v));
    }

    let key_entry =
        keys.iter()
            .find(|k| k.kid == claims.kid)
            .ok_or_else(|| ValidationError::UnknownKey {
                kid: claims.kid.clone(),
            })?;
    let key_bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&key_entry.public_key)
        .map_err(|_| ValidationError::Malformed)?
        .try_into()
        .map_err(|_| ValidationError::Malformed)?;
    let verifying_key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| ValidationError::Malformed)?;

    verifying_key
        .verify(&payload, &signature)
        .map_err(|_| ValidationError::BadSignature)?;

    // Only trust claims AFTER the signature checks out.
    if claims.exp < now_unix {
        return Err(ValidationError::Expired {
            expired_at_unix: claims.exp,
        });
    }
    if let Some(expected) = expected_trip
        && claims.trp != expected
    {
        return Err(ValidationError::WrongTrip);
    }

    Ok(VerifiedTicket {
        ticket_id: claims.tid,
        trip_id: claims.trp,
        unit_code: claims.unt,
        from_index: claims.f,
        to_index: claims.t,
        passenger_name: claims.pax,
        fare_class: claims.fc,
        expires_at_unix: claims.exp,
        kid: claims.kid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_token(claims: &TicketClaims, key: &SigningKey) -> String {
        let mut payload = Vec::new();
        ciborium::into_writer(claims, &mut payload).unwrap();
        let sig = key.sign(&payload);
        format!(
            "LT1.{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(sig.to_bytes())
        )
    }

    fn test_key() -> (SigningKey, Vec<KeyEntry>) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let entry = KeyEntry {
            kid: "test-key".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
        };
        (key, vec![entry])
    }

    fn claims() -> TicketClaims {
        TicketClaims {
            v: 1,
            tid: Uuid::new_v4(),
            trp: Uuid::new_v4(),
            unt: "12A".into(),
            f: 1,
            t: 3,
            pax: "Maria Santos".into(),
            fc: Some("economy".into()),
            exp: 2_000_000_000,
            kid: "test-key".into(),
        }
    }

    #[test]
    fn valid_ticket_verifies_and_carries_claims() {
        let (key, keys) = test_key();
        let c = claims();
        let token = make_token(&c, &key);
        let verified = verify_ticket(&token, &keys, 1_900_000_000, Some(c.trp)).unwrap();
        assert_eq!(verified.unit_code, "12A");
        assert_eq!(verified.passenger_name, "Maria Santos");
        assert_eq!((verified.from_index, verified.to_index), (1, 3));
    }

    #[test]
    fn tampered_payload_or_signature_is_rejected() {
        let (key, keys) = test_key();
        let token = make_token(&claims(), &key);

        // Flip a character in the signature part.
        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(matches!(
            verify_ticket(&tampered, &keys, 0, None),
            Err(ValidationError::BadSignature | ValidationError::Malformed)
        ));

        // Re-sign with a different key: unknown kid or bad signature.
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let forged = make_token(&claims(), &other);
        assert_eq!(
            verify_ticket(&forged, &keys, 0, None),
            Err(ValidationError::BadSignature)
        );
    }

    #[test]
    fn expiry_wrong_trip_and_unknown_key_are_rejected() {
        let (key, keys) = test_key();
        let c = claims();
        let token = make_token(&c, &key);

        assert!(matches!(
            verify_ticket(&token, &keys, 2_100_000_000, None),
            Err(ValidationError::Expired { .. })
        ));
        assert_eq!(
            verify_ticket(&token, &keys, 0, Some(Uuid::new_v4())),
            Err(ValidationError::WrongTrip)
        );
        assert!(matches!(
            verify_ticket(&token, &[], 0, None),
            Err(ValidationError::UnknownKey { .. })
        ));
    }

    #[test]
    fn garbage_tokens_are_malformed_not_panics() {
        let (_, keys) = test_key();
        for junk in ["", "LT1.", "LT1.a.b", "QR-JUNK", "LT9.aaaa.bbbb"] {
            assert!(verify_ticket(junk, &keys, 0, None).is_err(), "{junk:?}");
        }
    }
}
