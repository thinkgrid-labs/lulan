//! A payment provider described by configuration rather than code.
//!
//! Most payment APIs are the same three shapes wearing different names:
//! POST somewhere to create an intent, POST somewhere to refund it, and
//! receive an HMAC-signed callback. [`HttpProvider`] implements
//! [`PaymentProvider`](super::PaymentProvider) against a declarative
//! description of those three things, so integrating a PSP is a JSON file
//! — no Rust, no rebuild, no fork.
//!
//! This mirrors how pricing modules work: a runtime artifact the operator
//! supplies, not an image layer. Stripe ships as a built-in preset (see
//! [`preset`]) precisely to prove the description is expressive enough for
//! a real, large PSP; a provider that genuinely cannot be described this
//! way can still implement the trait directly in Rust.
//!
//! **Secrets never live in the config file** — fields name environment
//! variables, and the values are read from the process environment.
//!
//! ```json
//! {
//!   "name": "acme-pay",
//!   "base_url": "https://api.acme.test",
//!   "auth": { "type": "bearer", "token_env": "ACME_SECRET" },
//!   "encoding": "json",
//!   "create_intent": {
//!     "path": "/payments",
//!     "fields": { "amount": "{amount_minor}", "currency": "{currency}" },
//!     "intent_id_pointer": "/id"
//!   },
//!   "refund": { "path": "/refunds", "fields": { "id": "{payment_intent_id}" } },
//!   "webhook": {
//!     "signature_header": "x-acme-signature",
//!     "secret_env": "ACME_WEBHOOK_SECRET",
//!     "intent_id_pointer": "/payment/id",
//!     "event_type_pointer": "/event",
//!     "captured_events": ["payment.paid"],
//!     "failed_events": ["payment.failed"]
//!   }
//! }
//! ```

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Sha256, Sha512};
use uuid::Uuid;

use super::{BoxFuture, IntentStatus, PaymentError, PaymentEvent, PaymentIntent, PaymentProvider};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How request bodies are encoded. Form covers the older/large PSPs
/// (Stripe, PayPal classic); JSON covers most newer ones.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Json,
    Form,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    /// `Authorization: Bearer <secret>`
    Bearer {
        token_env: String,
    },
    /// `Authorization: Basic base64(<secret>:)` — the common "API key as
    /// username, empty password" convention.
    Basic {
        token_env: String,
        #[serde(default)]
        password_env: Option<String>,
    },
    /// Any single custom header, e.g. `X-Api-Key: <secret>`.
    Header {
        name: String,
        value_env: String,
    },
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    /// Appended to `base_url`.
    pub path: String,
    /// Body fields. Values are templates (see [`render`]); in JSON mode a
    /// dotted key nests (`"metadata.order_id"`), and a value that is
    /// exactly `{amount_minor}` or `{amount_major}` is emitted as a number.
    ///
    /// Amount placeholders: `{amount_minor}` (integer minor unit — Stripe
    /// and most; no conversion), `{amount_decimal}` (major-unit string,
    /// exponent-aware: `"450.00"`, `"5000"` for JPY, `"5.000"` for BHD —
    /// PayPal-style gateways), `{amount_major}` (integer major unit).
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// RFC 6901 JSON pointer to the provider's id for the intent in the
    /// response.
    #[serde(default)]
    pub intent_id_pointer: Option<String>,
    /// Optional pointer to a client-side confirmation token.
    #[serde(default)]
    pub client_secret_pointer: Option<String>,
    /// Header to send the order id in so the PSP dedupes our retries.
    #[serde(default)]
    pub idempotency_header: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Digest {
    #[default]
    Sha256,
    Sha512,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SignatureEncoding {
    #[default]
    Hex,
    Base64,
}

/// How to find the signature inside its header.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeaderFormat {
    /// The header value IS the digest.
    #[default]
    Raw,
    /// Comma-separated `key=value` pairs, Stripe-style
    /// (`t=1700000000,v1=abc…`). Several signature entries may appear;
    /// any match is accepted, which is how key rotation works.
    KeyValue {
        #[serde(default)]
        timestamp_key: Option<String>,
        signature_key: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Webhook {
    /// Header carrying the signature. Absent = the provider does not sign
    /// its callbacks, and the endpoint will demand an API key instead.
    #[serde(default)]
    pub signature_header: Option<String>,
    #[serde(default)]
    pub header_format: HeaderFormat,
    /// Env var holding the shared secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// What the HMAC is computed over. `{body}` and `{timestamp}` are
    /// substituted; the default is the raw body.
    #[serde(default = "default_signed_payload")]
    pub signed_payload: String,
    #[serde(default)]
    pub digest: Digest,
    #[serde(default)]
    pub encoding: SignatureEncoding,
    /// Reject signatures older than this, when a timestamp is available.
    /// 0 disables the check.
    #[serde(default = "default_tolerance")]
    pub tolerance_seconds: i64,
    /// Pointer to the provider's event name in the callback body.
    #[serde(default)]
    pub event_type_pointer: Option<String>,
    /// Pointer to the intent id the callback concerns.
    pub intent_id_pointer: String,
    /// Event names that mean "money captured" / "payment failed".
    /// Anything else authentic is acknowledged and ignored.
    #[serde(default)]
    pub captured_events: Vec<String>,
    #[serde(default)]
    pub failed_events: Vec<String>,
}

fn default_signed_payload() -> String {
    "{body}".to_string()
}
fn default_tolerance() -> i64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default = "default_auth")]
    pub auth: Auth,
    #[serde(default)]
    pub encoding: Encoding,
    pub create_intent: Call,
    pub refund: Call,
    pub webhook: Webhook,
    /// Request timeout for provider calls.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_auth() -> Auth {
    Auth::None
}
fn default_timeout() -> u64 {
    20
}

impl ProviderConfig {
    pub fn from_json(source: &str) -> Result<Self, String> {
        serde_json::from_str(source).map_err(|e| format!("payment provider config: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct HttpProvider {
    config: ProviderConfig,
    /// Leaked so `name()` can hand out `&'static str` like every other
    /// adapter; there is exactly one provider per process.
    name: &'static str,
    auth_header: Option<(String, String)>,
    webhook_secret: Option<String>,
    client: reqwest::Client,
}

impl HttpProvider {
    /// Resolve secrets from the environment and build the HTTP client.
    /// Fails loudly at boot rather than at the first sale.
    pub fn new(config: ProviderConfig) -> Result<Self, String> {
        let env = |key: &str| -> Result<String, String> {
            std::env::var(key)
                .map_err(|_| format!("payment provider {}: {key} is not set", config.name))
        };

        let auth_header = match &config.auth {
            Auth::Bearer { token_env } => Some((
                "authorization".to_string(),
                format!("Bearer {}", env(token_env)?),
            )),
            Auth::Basic {
                token_env,
                password_env,
            } => {
                let user = env(token_env)?;
                let password = match password_env {
                    Some(key) => env(key)?,
                    None => String::new(),
                };
                Some((
                    "authorization".to_string(),
                    format!("Basic {}", BASE64.encode(format!("{user}:{password}"))),
                ))
            }
            Auth::Header { name, value_env } => Some((name.to_lowercase(), env(value_env)?)),
            Auth::None => None,
        };

        let webhook_secret = match &config.webhook.secret_env {
            Some(key) => Some(env(key)?),
            None => None,
        };
        if config.webhook.signature_header.is_some() && webhook_secret.is_none() {
            return Err(format!(
                "payment provider {}: a signature header is configured but no secret_env",
                config.name
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| format!("payment provider {}: {e}", config.name))?;

        Ok(Self {
            name: Box::leak(config.name.clone().into_boxed_str()),
            config,
            auth_header,
            webhook_secret,
            client,
        })
    }

    async fn post(&self, call: &Call, vars: &Vars<'_>) -> Result<Value, PaymentError> {
        let url = format!(
            "{}{}",
            self.config.base_url.trim_end_matches('/'),
            call.path
        );
        let mut request = self.client.post(&url);
        if let Some((name, value)) = &self.auth_header {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(header) = &call.idempotency_header {
            request = request.header(header.as_str(), vars.idempotency_key.as_str());
        }
        request = match self.config.encoding {
            Encoding::Form => request.form(&form_body(&call.fields, vars)),
            Encoding::Json => request.json(&json_body(&call.fields, vars)),
        };

        let response = request
            .send()
            .await
            .map_err(|e| PaymentError::Unavailable(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            // A 2xx with an unparseable body is a provider problem, not a
            // decline — retrying may work.
            return serde_json::from_str(&body).map_err(|e| {
                PaymentError::Unavailable(format!("provider returned unparseable JSON: {e}"))
            });
        }
        // 4xx is an answer ("declined", "already refunded"); 5xx is an
        // outage. Only the first is final.
        let detail = provider_message(&body).unwrap_or_else(|| truncate(&body, 300));
        if status.is_client_error() {
            Err(PaymentError::Rejected(format!("HTTP {status}: {detail}")))
        } else {
            Err(PaymentError::Unavailable(format!(
                "HTTP {status}: {detail}"
            )))
        }
    }
}

impl PaymentProvider for HttpProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn authenticates_callbacks(&self) -> bool {
        self.config.webhook.signature_header.is_some() && self.webhook_secret.is_some()
    }

    fn create_intent<'a>(
        &'a self,
        order_id: Uuid,
        amount_minor: i64,
        currency: &'a str,
    ) -> BoxFuture<'a, Result<PaymentIntent, PaymentError>> {
        Box::pin(async move {
            let vars = Vars {
                order_id,
                amount_minor,
                currency,
                payment_intent_id: "",
                idempotency_key: order_id.to_string(),
            };
            let call = &self.config.create_intent;
            let body = self.post(call, &vars).await?;

            let pointer = call.intent_id_pointer.as_deref().unwrap_or("/id");
            // Most PSPs give string ids; a few use integers. Take either.
            let id = body
                .pointer(pointer)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .ok_or_else(|| {
                    PaymentError::Unavailable(format!(
                        "provider response has no intent id at {pointer}"
                    ))
                })?;
            let client_secret = call
                .client_secret_pointer
                .as_deref()
                .and_then(|p| body.pointer(p))
                .and_then(Value::as_str)
                .map(str::to_string);

            Ok(PaymentIntent {
                id,
                status: IntentStatus::Pending,
                client_secret,
            })
        })
    }

    fn refund<'a>(
        &'a self,
        payment_intent_id: &'a str,
        amount_minor: i64,
    ) -> BoxFuture<'a, Result<(), PaymentError>> {
        Box::pin(async move {
            let vars = Vars {
                order_id: Uuid::nil(),
                amount_minor,
                currency: "",
                payment_intent_id,
                // The amount is part of the key so a partial refund and a
                // later, different partial refund of the same intent are
                // distinct requests. A fixed per-intent key would make the
                // second one replay the first and silently not refund.
                idempotency_key: format!("refund-{payment_intent_id}-{amount_minor}"),
            };
            self.post(&self.config.refund, &vars).await?;
            Ok(())
        })
    }

    fn verify_callback(
        &self,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<PaymentEvent, PaymentError> {
        let webhook = &self.config.webhook;

        if webhook.signature_header.is_some() {
            let secret = self
                .webhook_secret
                .as_deref()
                .ok_or(PaymentError::BadSignature)?;
            let header = signature.ok_or(PaymentError::BadSignature)?;
            verify_signature(webhook, secret, header, body)?;
        }

        let payload: Value =
            serde_json::from_slice(body).map_err(|e| PaymentError::Malformed(e.to_string()))?;

        let event_type = webhook
            .event_type_pointer
            .as_deref()
            .and_then(|p| payload.pointer(p))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let captured = webhook.captured_events.iter().any(|e| e == &event_type);
        let failed = webhook.failed_events.iter().any(|e| e == &event_type);
        if !captured && !failed {
            return Ok(PaymentEvent::Ignored);
        }

        let payment_intent_id = payload
            .pointer(&webhook.intent_id_pointer)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PaymentError::Malformed(format!(
                    "callback has no intent id at {}",
                    webhook.intent_id_pointer
                ))
            })?
            .to_string();

        Ok(if captured {
            PaymentEvent::Captured { payment_intent_id }
        } else {
            PaymentEvent::Failed { payment_intent_id }
        })
    }
}

// ---------------------------------------------------------------------------
// Templating, bodies, signatures
// ---------------------------------------------------------------------------

struct Vars<'a> {
    order_id: Uuid,
    amount_minor: i64,
    currency: &'a str,
    payment_intent_id: &'a str,
    idempotency_key: String,
}

/// ISO 4217 fractional digits for a currency, so the adapter can present
/// the amount the way each PSP expects. Lulan stores money in ONE internal
/// unit (the ISO minor unit) everywhere; this only affects what crosses the
/// wire to a provider.
///
/// Stripe and most modern APIs want the minor unit as an integer, which is
/// what Lulan already holds, so `{amount_minor}` needs no conversion. But
/// plenty of gateways (PayPal, many regional processors) want a major-unit
/// decimal string (`"450.00"`), and for that the exponent has to be right —
/// `¥5000` is `"5000"`, not `"50.00"`, and `BD 5` is `"5.000"`.
///
/// The lists are the standard exceptions; everything unlisted is 2.
pub fn iso_exponent(currency: &str) -> u32 {
    match currency.to_uppercase().as_str() {
        // Zero-decimal: minor unit == major unit.
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
        // Three-decimal.
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

/// Format `amount_minor` as a major-unit decimal string for the currency,
/// e.g. `45000` PHP → `"450.00"`, `5000` JPY → `"5000"`, `5000` BHD →
/// `"5.000"`.
fn amount_decimal(amount_minor: i64, currency: &str) -> String {
    let exp = iso_exponent(currency);
    if exp == 0 {
        return amount_minor.to_string();
    }
    let scale = 10i64.pow(exp);
    let sign = if amount_minor < 0 { "-" } else { "" };
    let n = amount_minor.unsigned_abs();
    let major = n / scale as u64;
    let frac = n % scale as u64;
    format!("{sign}{major}.{frac:0width$}", width = exp as usize)
}

/// The integer major unit (truncating), for the rare PSP that wants it.
fn amount_major(amount_minor: i64, currency: &str) -> i64 {
    amount_minor / 10i64.pow(iso_exponent(currency))
}

/// Substitute `{placeholders}` in a field template.
fn render(template: &str, vars: &Vars<'_>) -> String {
    template
        .replace("{order_id}", &vars.order_id.to_string())
        .replace("{amount_minor}", &vars.amount_minor.to_string())
        .replace(
            "{amount_decimal}",
            &amount_decimal(vars.amount_minor, vars.currency),
        )
        .replace(
            "{amount_major}",
            &amount_major(vars.amount_minor, vars.currency).to_string(),
        )
        .replace("{currency}", vars.currency)
        .replace("{currency_lower}", &vars.currency.to_lowercase())
        .replace("{currency_upper}", &vars.currency.to_uppercase())
        .replace("{payment_intent_id}", vars.payment_intent_id)
}

fn form_body(fields: &BTreeMap<String, String>, vars: &Vars<'_>) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(k, v)| (k.clone(), render(v, vars)))
        .collect()
}

/// Dotted keys nest. A value that is exactly `{amount_minor}` or
/// `{amount_major}` becomes a JSON number (providers reject a string
/// there); `{amount_decimal}` stays a string, which is what the gateways
/// that use it want (`"value": "10.00"`).
fn json_body(fields: &BTreeMap<String, String>, vars: &Vars<'_>) -> Value {
    let mut root = json!({});
    for (key, template) in fields {
        let value = match template.trim() {
            "{amount_minor}" => json!(vars.amount_minor),
            "{amount_major}" => json!(amount_major(vars.amount_minor, vars.currency)),
            _ => json!(render(template, vars)),
        };
        let mut cursor = &mut root;
        let parts: Vec<&str> = key.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            cursor = cursor
                .as_object_mut()
                .expect("cursor is always an object")
                .entry((*part).to_string())
                .or_insert_with(|| json!({}));
            if !cursor.is_object() {
                *cursor = json!({});
            }
        }
        cursor
            .as_object_mut()
            .expect("cursor is always an object")
            .insert(parts[parts.len() - 1].to_string(), value);
    }
    root
}

fn verify_signature(
    webhook: &Webhook,
    secret: &str,
    header: &str,
    body: &[u8],
) -> Result<(), PaymentError> {
    // Pull the candidate digests (and timestamp) out of the header.
    let (timestamp, candidates): (Option<i64>, Vec<String>) = match &webhook.header_format {
        HeaderFormat::Raw => (None, vec![header.trim().to_string()]),
        HeaderFormat::KeyValue {
            timestamp_key,
            signature_key,
        } => {
            let mut timestamp = None;
            let mut signatures = Vec::new();
            for part in header.split(',') {
                let Some((key, value)) = part.split_once('=') else {
                    continue;
                };
                let (key, value) = (key.trim(), value.trim());
                if Some(key) == timestamp_key.as_deref() {
                    timestamp = value.parse::<i64>().ok();
                } else if key == signature_key {
                    signatures.push(value.to_string());
                }
            }
            (timestamp, signatures)
        }
    };
    if candidates.is_empty() {
        return Err(PaymentError::BadSignature);
    }

    // A signature that is valid forever is a replay waiting to happen.
    if webhook.tolerance_seconds > 0
        && let Some(timestamp) = timestamp
    {
        let age = chrono::Utc::now().timestamp() - timestamp;
        if age.abs() > webhook.tolerance_seconds {
            return Err(PaymentError::BadSignature);
        }
    }

    // Build the signed payload as BYTES: the raw body is HMAC'd exactly as
    // received. Going through a lossy String would mangle any non-UTF-8
    // byte and break verification — providers sign the wire bytes, not a
    // re-encoding of them.
    let signed = signed_payload_bytes(&webhook.signed_payload, timestamp.unwrap_or_default(), body);

    let expected = match webhook.digest {
        Digest::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .map_err(|_| PaymentError::BadSignature)?;
            mac.update(&signed);
            mac.finalize().into_bytes().to_vec()
        }
        Digest::Sha512 => {
            let mut mac = Hmac::<Sha512>::new_from_slice(secret.as_bytes())
                .map_err(|_| PaymentError::BadSignature)?;
            mac.update(&signed);
            mac.finalize().into_bytes().to_vec()
        }
    };
    let expected = match webhook.encoding {
        SignatureEncoding::Hex => expected.iter().map(|b| format!("{b:02x}")).collect(),
        SignatureEncoding::Base64 => BASE64.encode(&expected),
    };

    // Constant-time compare against every offered signature (rotation
    // sends more than one), and never short-circuit on the first match.
    let mut matched = false;
    for candidate in &candidates {
        matched |= constant_time_eq(candidate.trim(), &expected);
    }
    if matched {
        Ok(())
    } else {
        Err(PaymentError::BadSignature)
    }
}

/// Render the signed-payload template to bytes: `{timestamp}` (digits,
/// safe as text) is substituted, and every `{body}` is spliced in as the
/// raw request bytes.
fn signed_payload_bytes(template: &str, timestamp: i64, body: &[u8]) -> Vec<u8> {
    let templated = template.replace("{timestamp}", &timestamp.to_string());
    let mut out = Vec::with_capacity(templated.len() + body.len());
    let mut rest = templated.as_bytes();
    let marker = b"{body}";
    while let Some(idx) = rest.windows(marker.len()).position(|w| w == marker) {
        out.extend_from_slice(&rest[..idx]);
        out.extend_from_slice(body);
        rest = &rest[idx + marker.len()..];
    }
    out.extend_from_slice(rest);
    out
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Most PSPs put a human-readable reason somewhere obvious; surface it so
/// operators are not left reading raw JSON in a log.
fn provider_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    for pointer in [
        "/error/message",
        "/message",
        "/error",
        "/errors/0/detail",
        "/detail",
    ] {
        if let Some(Value::String(message)) = value.pointer(pointer) {
            return Some(truncate(message, 300));
        }
    }
    None
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

/// Configurations for providers common enough to ship. Selecting one by
/// name is the whole integration; the operator supplies only secrets.
///
/// Anything not here is a JSON file away — that is the point.
pub mod preset {
    /// Every preset's name, for error messages and `--help`-ish output.
    pub const NAMES: [&str; 1] = ["stripe"];

    pub fn by_name(name: &str) -> Option<&'static str> {
        match name {
            "stripe" => Some(STRIPE),
            _ => None,
        }
    }

    /// Stripe PaymentIntents. Secrets: `LULAN_PAYMENT_SECRET` (the
    /// `sk_…` key) and `LULAN_PAYMENT_WEBHOOK_SECRET` (the `whsec_…`
    /// endpoint secret).
    ///
    /// Amounts pass through as minor units, which is Stripe's own
    /// convention — including for zero-decimal currencies like JPY, where
    /// "minor units" means whole yen on both sides.
    pub const STRIPE: &str = r#"{
  "name": "stripe",
  "base_url": "https://api.stripe.com",
  "encoding": "form",
  "auth": { "type": "bearer", "token_env": "LULAN_PAYMENT_SECRET" },
  "create_intent": {
    "path": "/v1/payment_intents",
    "fields": {
      "amount": "{amount_minor}",
      "currency": "{currency_lower}",
      "metadata[order_id]": "{order_id}",
      "automatic_payment_methods[enabled]": "true"
    },
    "intent_id_pointer": "/id",
    "client_secret_pointer": "/client_secret",
    "idempotency_header": "Idempotency-Key"
  },
  "refund": {
    "path": "/v1/refunds",
    "fields": {
      "payment_intent": "{payment_intent_id}",
      "amount": "{amount_minor}"
    },
    "intent_id_pointer": "/id",
    "idempotency_header": "Idempotency-Key"
  },
  "webhook": {
    "signature_header": "stripe-signature",
    "header_format": { "type": "key_value", "timestamp_key": "t", "signature_key": "v1" },
    "secret_env": "LULAN_PAYMENT_WEBHOOK_SECRET",
    "signed_payload": "{timestamp}.{body}",
    "digest": "sha256",
    "encoding": "hex",
    "tolerance_seconds": 300,
    "event_type_pointer": "/type",
    "intent_id_pointer": "/data/object/id",
    "captured_events": ["payment_intent.succeeded"],
    "failed_events": ["payment_intent.payment_failed", "payment_intent.canceled"]
  }
}"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponents_cover_the_iso_exceptions() {
        assert_eq!(iso_exponent("PHP"), 2);
        assert_eq!(iso_exponent("usd"), 2); // case-insensitive
        assert_eq!(iso_exponent("JPY"), 0);
        assert_eq!(iso_exponent("KRW"), 0);
        assert_eq!(iso_exponent("BHD"), 3);
        assert_eq!(iso_exponent("ZZZ"), 2); // unknown → the common default
    }

    /// The whole point of Finding 1: an amount is formatted for the
    /// currency's real decimal places, so a decimal-wanting PSP charges
    /// the right number.
    #[test]
    fn amount_decimal_respects_currency_exponent() {
        assert_eq!(amount_decimal(45000, "PHP"), "450.00");
        assert_eq!(amount_decimal(45067, "PHP"), "450.67");
        assert_eq!(amount_decimal(4507, "PHP"), "45.07"); // fraction zero-padded
        // Zero-decimal: NOT "50.00" — ¥5000 is five thousand yen.
        assert_eq!(amount_decimal(5000, "JPY"), "5000");
        // Three-decimal.
        assert_eq!(amount_decimal(5000, "BHD"), "5.000");
        assert_eq!(amount_decimal(5123, "BHD"), "5.123");
    }

    #[test]
    fn amount_major_truncates_to_the_major_unit() {
        assert_eq!(amount_major(45099, "PHP"), 450);
        assert_eq!(amount_major(5000, "JPY"), 5000);
        assert_eq!(amount_major(5999, "BHD"), 5);
    }

    /// Stripe's preset uses {amount_minor}, which must stay the raw integer
    /// — Stripe's own convention IS ISO minor units, so no conversion.
    #[test]
    fn stripe_amount_is_the_untouched_minor_integer() {
        let vars = Vars {
            order_id: uuid::Uuid::nil(),
            amount_minor: 5000,
            currency: "JPY",
            payment_intent_id: "",
            idempotency_key: String::new(),
        };
        let mut fields = BTreeMap::new();
        fields.insert("amount".to_string(), "{amount_minor}".to_string());
        let body = json_body(&fields, &vars);
        assert_eq!(body["amount"], serde_json::json!(5000)); // number, not "50.00"
    }

    /// The HMAC input is built over raw bytes, so a body with a non-UTF-8
    /// byte still verifies (Finding 4). A lossy String would have changed
    /// the 0xFF into the replacement char and broken the digest.
    #[test]
    fn signed_payload_preserves_raw_bytes() {
        let body = [0x7b, 0xff, 0x7d]; // { <0xFF> }  — not valid UTF-8
        let signed = signed_payload_bytes("{timestamp}.{body}", 1700000000, &body);
        assert!(
            signed.ends_with(&body),
            "the raw body bytes must survive intact"
        );
        assert!(signed.starts_with(b"1700000000."));
    }
}
