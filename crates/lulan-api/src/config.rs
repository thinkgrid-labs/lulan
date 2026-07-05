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
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            listen_addr: std::env::var("LULAN_LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: std::env::var("DATABASE_URL").ok(),
            redis_url: std::env::var("REDIS_URL").ok(),
        }
    }
}
