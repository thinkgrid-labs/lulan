use lulan_engine::inventory::{HoldStore, InventoryStore};
use redis::aio::ConnectionManager;
use sqlx::PgPool;

use crate::error::ApiError;

#[derive(Clone)]
pub struct AppState {
    /// Optional so the server can boot infra-less (health endpoints, CI
    /// smoke tests). Endpoints that need the database return 503 without it.
    pub db: Option<PgPool>,
    /// Optional by design, not just for dev: holds degrade to 503 while
    /// claims stay correct (ADR 0002).
    pub redis: Option<ConnectionManager>,
}

impl AppState {
    pub fn inventory(&self) -> Result<InventoryStore, ApiError> {
        self.db
            .clone()
            .map(InventoryStore::new)
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
