//! Payments are a port, not a feature: the engine defines the contract and
//! reconciles provider callbacks against the order state machine. It never
//! touches card data, and it never learns a provider's vocabulary — an
//! adapter translates, the state machine only ever sees "captured" or
//! "failed".
//!
//! Providers are **configuration, not code**: [`http::HttpProvider`]
//! implements this trait against a JSON description of a PSP's create /
//! refund / webhook endpoints, so integrating one is a file rather than a
//! fork. Stripe ships as a built-in preset ([`http::preset`]) to prove the
//! description handles a real, large PSP. [`FakeProvider`] drives dev and
//! tests. Anything the description cannot express implements this trait
//! directly in Rust — the escape hatch, not the expected path.
//!
//! The trait is deliberately object-safe (boxed futures rather than
//! `-> impl Future`) so the running provider is a configuration choice,
//! `Arc<dyn PaymentProvider>` in app state, exactly like the pricing
//! engine and the identity provider.

pub mod http;

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;
use uuid::Uuid;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    /// The provider understood the request and said no (declined card,
    /// already-refunded charge, bad parameters).
    #[error("payment provider rejected the request: {0}")]
    Rejected(String),
    /// The provider could not be reached, or answered with something
    /// unusable. Retryable.
    #[error("could not reach the payment provider: {0}")]
    Unavailable(String),
    /// A callback did not carry a valid signature. Treat as hostile.
    #[error("callback signature is missing or invalid")]
    BadSignature,
    #[error("callback payload is malformed: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    /// Created; awaiting customer action / provider confirmation.
    Pending,
}

/// A provider-side payment intent for one order.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentIntent {
    /// Provider-scoped id (`fake_pi_…`, `pi_…` for Stripe). This is the
    /// key callbacks are reconciled against, stored on the order.
    pub id: String,
    pub status: IntentStatus,
    /// Client-side confirmation token, when the provider uses one (Stripe
    /// calls it `client_secret`). The storefront needs it to collect card
    /// details; it is not a server credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// A verified callback, normalised to what the order state machine can
/// act on. Anything else a provider chooses to send is [`Ignored`].
///
/// [`Ignored`]: PaymentEvent::Ignored
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentEvent {
    Captured {
        payment_intent_id: String,
    },
    Failed {
        payment_intent_id: String,
    },
    /// Understood, authentic, and irrelevant to the order lifecycle.
    /// Acknowledged with 200 so the provider stops retrying.
    Ignored,
}

pub trait PaymentProvider: Send + Sync + 'static {
    /// Short identifier for logs and operator-facing output.
    fn name(&self) -> &'static str;

    /// Whether the provider authenticates its own callbacks by signing
    /// them. When false, the webhook endpoint must demand an API key
    /// instead — an unauthenticated capture endpoint is free travel.
    fn authenticates_callbacks(&self) -> bool;

    /// Create an intent to charge `amount_minor` (ISO 4217 minor units).
    fn create_intent<'a>(
        &'a self,
        order_id: Uuid,
        amount_minor: i64,
        currency: &'a str,
    ) -> BoxFuture<'a, Result<PaymentIntent, PaymentError>>;

    /// Refund a captured intent. Providers that refund asynchronously
    /// should still return Ok once the refund is accepted.
    fn refund<'a>(
        &'a self,
        payment_intent_id: &'a str,
        amount_minor: i64,
    ) -> BoxFuture<'a, Result<(), PaymentError>>;

    /// Verify a raw callback and say what it means. Verification and
    /// interpretation live together on purpose: an adapter must not be
    /// able to report an event it has not authenticated.
    fn verify_callback(
        &self,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<PaymentEvent, PaymentError>;
}

/// Dev/test provider: intents always succeed, callbacks are unsigned JSON
/// (`{"payment_intent_id": "...", "status": "succeeded"|"failed"}`) posted
/// by hand or by the test suite.
///
/// It cannot authenticate its own callbacks, so the webhook endpoint
/// requires an integration API key while this provider is active.
#[derive(Debug, Clone, Default)]
pub struct FakeProvider;

#[derive(serde::Deserialize)]
struct FakeCallback {
    payment_intent_id: String,
    status: String,
}

impl PaymentProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn authenticates_callbacks(&self) -> bool {
        false
    }

    fn create_intent<'a>(
        &'a self,
        _order_id: Uuid,
        _amount_minor: i64,
        _currency: &'a str,
    ) -> BoxFuture<'a, Result<PaymentIntent, PaymentError>> {
        Box::pin(async move {
            Ok(PaymentIntent {
                id: format!("fake_pi_{}", Uuid::new_v4().simple()),
                status: IntentStatus::Pending,
                client_secret: None,
            })
        })
    }

    fn refund<'a>(
        &'a self,
        _payment_intent_id: &'a str,
        _amount_minor: i64,
    ) -> BoxFuture<'a, Result<(), PaymentError>> {
        Box::pin(async move { Ok(()) })
    }

    fn verify_callback(
        &self,
        _signature: Option<&str>,
        body: &[u8],
    ) -> Result<PaymentEvent, PaymentError> {
        let callback: FakeCallback =
            serde_json::from_slice(body).map_err(|e| PaymentError::Malformed(e.to_string()))?;
        match callback.status.as_str() {
            "succeeded" => Ok(PaymentEvent::Captured {
                payment_intent_id: callback.payment_intent_id,
            }),
            "failed" => Ok(PaymentEvent::Failed {
                payment_intent_id: callback.payment_intent_id,
            }),
            other => Err(PaymentError::Malformed(format!(
                "unknown payment status {other:?} (expected succeeded or failed)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_callbacks_map_to_lifecycle_events() {
        let provider = FakeProvider;
        assert_eq!(
            provider
                .verify_callback(
                    None,
                    br#"{"payment_intent_id":"pi_1","status":"succeeded"}"#
                )
                .unwrap(),
            PaymentEvent::Captured {
                payment_intent_id: "pi_1".into()
            }
        );
        assert_eq!(
            provider
                .verify_callback(None, br#"{"payment_intent_id":"pi_1","status":"failed"}"#)
                .unwrap(),
            PaymentEvent::Failed {
                payment_intent_id: "pi_1".into()
            }
        );
        assert!(matches!(
            provider.verify_callback(None, br#"{"payment_intent_id":"pi_1","status":"???"}"#),
            Err(PaymentError::Malformed(_))
        ));
        assert!(matches!(
            provider.verify_callback(None, b"not json"),
            Err(PaymentError::Malformed(_))
        ));
    }

    /// The security property the webhook endpoint depends on.
    #[test]
    fn fake_provider_admits_it_cannot_authenticate_callbacks() {
        assert!(!FakeProvider.authenticates_callbacks());
    }
}
