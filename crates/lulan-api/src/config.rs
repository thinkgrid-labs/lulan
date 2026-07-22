/// Server configuration, read from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Listen address, e.g. `0.0.0.0:8080`. Env: `LULAN_LISTEN_ADDR`.
    pub listen_addr: String,
    /// Postgres connection string. Env: `DATABASE_URL`. Optional in early
    /// phases so the server can boot without infrastructure.
    pub database_url: Option<String>,
    /// Redis connection string for soft holds. Env: `REDIS_URL`. Optional:
    /// without it, hold endpoints return 503 but claims still work — Redis
    /// is never required for correctness (ADR 0002).
    pub redis_url: Option<String>,
    /// Path to an operator pricing module (`.wasm`). Env:
    /// `LULAN_PRICING_WASM`. Unset = native rule engine.
    pub pricing_wasm: Option<String>,
    /// HMAC key for quote tokens. Env: `LULAN_QUOTE_SECRET`. Unset = a
    /// random per-boot secret (quotes won't survive restarts).
    pub quote_secret: Option<String>,
    /// Payment provider: a built-in preset name (`stripe`) or a path to a
    /// provider description. Env: `LULAN_PAYMENT_PROVIDER`. Unset = the
    /// fake provider, which takes no money.
    pub payment_provider: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            listen_addr: std::env::var("LULAN_LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: std::env::var("DATABASE_URL").ok(),
            redis_url: std::env::var("REDIS_URL").ok(),
            pricing_wasm: std::env::var("LULAN_PRICING_WASM").ok(),
            quote_secret: std::env::var("LULAN_QUOTE_SECRET").ok(),
            payment_provider: std::env::var("LULAN_PAYMENT_PROVIDER").ok(),
        }
    }
}
