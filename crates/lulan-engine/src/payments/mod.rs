//! Payments are a port, not a feature: the engine defines the contract and
//! reconciles provider webhooks against the order state machine. Real
//! adapters (Stripe, Xendit/PayMongo) implement [`PaymentProvider`];
//! [`FakeProvider`] drives dev, tests, and the Phase 3 exit criteria.

use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("payment provider error: {0}")]
pub struct PaymentError(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    /// Created; awaiting customer action / provider confirmation.
    Pending,
}

/// A provider-side payment intent for one order.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentIntent {
    /// Provider-scoped id (e.g. `fake_pi_…`, `pi_…` for Stripe).
    pub id: String,
    pub status: IntentStatus,
}

pub trait PaymentProvider: Send + Sync {
    /// Create an intent to charge `amount_minor` (ISO 4217 minor units).
    fn create_intent(
        &self,
        order_id: Uuid,
        amount_minor: i64,
        currency: &str,
    ) -> impl Future<Output = Result<PaymentIntent, PaymentError>> + Send;

    /// Refund a captured intent in full. Providers that refund
    /// asynchronously should still return Ok once the refund is accepted.
    fn refund(
        &self,
        payment_intent_id: &str,
        amount_minor: i64,
    ) -> impl Future<Output = Result<(), PaymentError>> + Send;
}

/// Dev/test provider: always succeeds at intent creation and "notifies" via
/// the fake webhook endpoint, exactly like a real provider's async flow.
#[derive(Debug, Clone, Default)]
pub struct FakeProvider;

impl PaymentProvider for FakeProvider {
    async fn create_intent(
        &self,
        _order_id: Uuid,
        _amount_minor: i64,
        _currency: &str,
    ) -> Result<PaymentIntent, PaymentError> {
        Ok(PaymentIntent {
            id: format!("fake_pi_{}", Uuid::new_v4().simple()),
            status: IntentStatus::Pending,
        })
    }

    async fn refund(
        &self,
        _payment_intent_id: &str,
        _amount_minor: i64,
    ) -> Result<(), PaymentError> {
        Ok(())
    }
}
