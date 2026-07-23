//! `@lulan/validate` — offline Lulan ticket verification for JavaScript.
//!
//! A thin wasm-bindgen layer over the pure Rust `lulan-validate` core, so
//! the exact code that verifies a ticket in the server verifies it in a
//! browser, a React Native boarding app, or Node — with no server call.
//!
//! The core crate deliberately carries no wasm-bindgen dependency (CI
//! builds it for `wasm32-unknown-unknown` without one); this crate adds
//! the JS boundary and nothing else. Keys and revocations cross that
//! boundary as plain JS values via serde; results come back as objects, or
//! a thrown `Error` whose `.message` is the machine-readable reason
//! (`expired`, `bad_signature`, `revoked`, …) so callers can branch on it.

use serde::Deserialize;
use wasm_bindgen::prelude::*;

#[derive(Deserialize)]
struct KeyEntryJs {
    kid: String,
    public_key: String,
}

impl From<KeyEntryJs> for lulan_validate::KeyEntry {
    fn from(k: KeyEntryJs) -> Self {
        lulan_validate::KeyEntry {
            kid: k.kid,
            public_key: k.public_key,
        }
    }
}

/// Verify a scanned ticket token.
///
/// - `token`: the scanned `LT1.…` string.
/// - `keys`: the key set from `GET /v1/ticket-keys`
///   (`[{ kid, public_key }]`), cached on the device.
/// - `now_unix`: current time in unix seconds, e.g. `Date.now() / 1000`
///   (the caller owns the clock).
/// - `expected_trip`: the trip being boarded, or `null`/`undefined` for a
///   generic inspection scan.
///
/// Resolves to the verified ticket; throws on any failure, with the reason
/// as the error message.
#[wasm_bindgen(js_name = verifyTicket)]
pub fn verify_ticket(
    token: &str,
    keys: JsValue,
    now_unix: f64,
    expected_trip: Option<String>,
) -> Result<JsValue, JsValue> {
    verify_ticket_with_revocations(token, keys, now_unix, expected_trip, JsValue::UNDEFINED)
}

/// As [`verifyTicket`], and additionally refuse anything on the operator's
/// revocation list (`GET /v1/revocations`, cached on the device).
///
/// A signature proves a ticket was issued, never that it is still valid: a
/// refund happens after signing, so the revocation list is the only way a
/// gate learns of it offline. `revoked` is an array of ticket-id strings;
/// pass `null`/`undefined` or `[]` when you have none.
///
/// [`verifyTicket`]: verify_ticket
#[wasm_bindgen(js_name = verifyTicketWithRevocations)]
pub fn verify_ticket_with_revocations(
    token: &str,
    keys: JsValue,
    now_unix: f64,
    expected_trip: Option<String>,
    revoked: JsValue,
) -> Result<JsValue, JsValue> {
    let keys: Vec<KeyEntryJs> =
        serde_wasm_bindgen::from_value(keys).map_err(|e| err("bad_key_set", &e.to_string()))?;
    let keys: Vec<lulan_validate::KeyEntry> = keys.into_iter().map(Into::into).collect();

    let expected = match expected_trip {
        Some(s) if !s.is_empty() => {
            Some(uuid_from_str(&s).map_err(|e| err("bad_expected_trip", &e))?)
        }
        _ => None,
    };

    let revoked_ids = parse_revoked(revoked)?;

    let verified = lulan_validate::verify_ticket_with_revocations(
        token,
        &keys,
        now_unix as i64,
        expected,
        &revoked_ids,
    )
    .map_err(validation_error_to_js)?;

    serde_wasm_bindgen::to_value(&verified).map_err(|e| err("internal", &e.to_string()))
}

fn parse_revoked(revoked: JsValue) -> Result<Vec<lulan_validate::Uuid>, JsValue> {
    if revoked.is_null() || revoked.is_undefined() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = serde_wasm_bindgen::from_value(revoked)
        .map_err(|e| err("bad_revocations", &e.to_string()))?;
    ids.iter()
        .map(|s| uuid_from_str(s).map_err(|e| err("bad_revocations", &e)))
        .collect()
}

fn uuid_from_str(s: &str) -> Result<lulan_validate::Uuid, String> {
    lulan_validate::Uuid::parse_str(s).map_err(|_| format!("{s:?} is not a valid UUID"))
}

/// Map the core's typed error to a thrown JS `Error`. The message is the
/// serde tag (`expired`, `bad_signature`, `unknown_key`, …), so callers
/// can `switch` on `err.message` rather than parse prose.
fn validation_error_to_js(e: lulan_validate::ValidationError) -> JsValue {
    use lulan_validate::ValidationError as V;
    let code = match e {
        V::Malformed => "malformed",
        V::UnsupportedVersion(_) => "unsupported_version",
        V::UnknownKey { .. } => "unknown_key",
        V::BadSignature => "bad_signature",
        V::Expired { .. } => "expired",
        V::WrongTrip => "wrong_trip",
        V::Revoked => "revoked",
    };
    err(code, &e.to_string())
}

fn err(code: &str, detail: &str) -> JsValue {
    js_sys::Error::new(&format!("{code}: {detail}")).into()
}
