use std::sync::Arc;

use lulan_engine::inventory::{HoldStore, InventoryStore};
use lulan_engine::orders::OrderStore;
use lulan_engine::payments::{FakeProvider, PaymentProvider};
use lulan_pricing::{NativeEngine, PricingEngine};
use redis::aio::ConnectionManager;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;

#[derive(Clone)]
pub struct AppState {
    /// Optional so the server can boot infra-less (health endpoints, CI
    /// smoke tests). Endpoints that need the database return 503 without it.
    pub db: Option<PgPool>,
    /// Optional by design, not just for dev: holds degrade to 503 while
    /// claims stay correct (ADR 0002).
    pub redis: Option<ConnectionManager>,
    /// The active pricing engine — native by default, an operator WASM
    /// module when `LULAN_PRICING_WASM` is set (ADR 0003).
    pub pricing: Arc<dyn PricingEngine>,
    /// The active payment provider (ADR-style port): a configured PSP, or
    /// FakeProvider when none is set. Never constructed inline by a
    /// handler — the running adapter is deployment configuration.
    pub payments: Arc<dyn PaymentProvider>,
    /// HMAC key for quote tokens.
    pub quote_secret: Arc<Vec<u8>>,
    /// Customer identity port (operator's IdP). None = guest checkout
    /// only; customer endpoints return 401.
    pub identity: Option<Arc<dyn crate::identity::IdentityProvider>>,
}

impl AppState {
    /// Native pricing engine and an ephemeral quote secret — what tests
    /// and infra-less boots want. Loads/creates the ticket signing key
    /// when a database is present.
    pub async fn new(db: Option<PgPool>, redis: Option<ConnectionManager>) -> Self {
        // Ensure a signing key exists; issuance reads the ACTIVE key at
        // sign time so rotation lands without a restart.
        if let Some(pool) = &db
            && let Err(err) = lulan_engine::ticket::TicketSigner::load_or_create(pool).await
        {
            tracing::error!(error = %err, "ticket signing key unavailable");
        }
        let identity: Option<Arc<dyn crate::identity::IdentityProvider>> =
            crate::identity::HsJwtIdentity::from_env()
                .map(|provider| Arc::new(provider) as Arc<dyn crate::identity::IdentityProvider>);
        Self {
            db,
            redis,
            pricing: Arc::new(NativeEngine),
            payments: Arc::new(FakeProvider),
            quote_secret: Arc::new(ephemeral_secret()),
            identity,
        }
    }

    pub fn inventory(&self) -> Result<InventoryStore, ApiError> {
        self.db
            .clone()
            .map(InventoryStore::new)
            .ok_or(ApiError::ServiceUnavailable("database not configured"))
    }

    pub fn orders(&self) -> Result<OrderStore, ApiError> {
        self.db
            .clone()
            .map(OrderStore::new)
            .ok_or(ApiError::ServiceUnavailable("database not configured"))
    }

    pub fn holds(&self) -> Result<HoldStore, ApiError> {
        self.redis
            .clone()
            .map(HoldStore::new)
            .ok_or(ApiError::ServiceUnavailable(
                "hold service unavailable (no Redis)",
            ))
    }
}

/// 32 random bytes. Quotes signed with an ephemeral secret die on restart,
/// which is fine — clients just re-quote.
pub fn ephemeral_secret() -> Vec<u8> {
    let mut secret = Uuid::new_v4().into_bytes().to_vec();
    secret.extend_from_slice(&Uuid::new_v4().into_bytes());
    secret
}
